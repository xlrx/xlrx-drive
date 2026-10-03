//! Text from file content for the search (PLAN 6.1, 6.6) – everything on the NAS.
//!
//! The search index queues a job for every content without text that can have some. Workers
//! take the jobs, read a stable copy of the file (verified against the content hash, so a text is
//! never stored under the wrong content) and store its text in `content_text`, once per content.
//!
//! - text files: read directly
//! - PDF: `pdftotext`; scans without a text layer: `pdftoppm` + `tesseract`
//! - TIFF scans: `tesseract`
//! - Office, iWork, OpenDocument, mail: Apache Tika (a container in the home network)
//!
//! Nothing is sent outside the NAS, so this is allowed for folders marked "Nur lokal" too.

pub mod plain;
pub mod tika;
pub mod tools;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::io::AsyncReadExt;

use crate::files::content::{self, Staged};
use crate::files::db::{self, NODE_COLS, NodeRow};
use crate::files::roots;
use crate::jobs::{self, Job};
use crate::search::schema::extension;
use crate::search::text;
use crate::state::AppState;
use tika::Tika;
use tools::{Failure, Tools};

/// Job kind in the queue; the key is the content hash in hex.
pub const KIND: &str = "extract";

/// Larger files are not read (the text of a 500-MB file is rarely what people look for).
pub const MAX_INPUT: i64 = 512 * 1024 * 1024;
/// Text files are read up to this many bytes.
const MAX_PLAIN: u64 = 8 * 1024 * 1024;
/// A PDF with fewer letters and digits per page than this has no real text layer: a scan (a few
/// stray characters, such as page numbers, do not count as text).
const SCAN_BELOW_PER_PAGE: usize = 20;
/// Without notification, workers look for jobs this often.
const POLL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Plain,
    Pdf,
    Scan,
    Tika,
}

/// How the text of a file with this extension is obtained, if it has one.
pub fn method(ext: &str) -> Option<Method> {
    Some(match ext {
        "txt" | "md" | "markdown" | "rst" | "log" | "csv" | "tsv" | "json" | "xml" | "yaml"
        | "yml" | "toml" | "ini" | "conf" | "css" | "js" | "ts" | "rs" | "py" | "swift"
        | "java" | "kt" | "c" | "h" | "cpp" | "hpp" | "go" | "rb" | "php" | "sh" | "sql"
        | "tex" | "vcf" | "ics" => Method::Plain,
        "pdf" => Method::Pdf,
        "tif" | "tiff" => Method::Scan,
        "doc" | "docx" | "docm" | "dot" | "dotx" | "odt" | "ott" | "rtf" | "pages" | "wpd"
        | "wps" | "xls" | "xlsx" | "xlsm" | "xlsb" | "ods" | "numbers" | "ppt" | "pptx" | "pps"
        | "ppsx" | "odp" | "key" | "eml" | "msg" | "epub" | "html" | "htm" => Method::Tika,
        _ => return None,
    })
}

/// Should the index ask for the text of this file?
pub fn wanted(name: &str, size: Option<i64>) -> bool {
    size.is_some_and(|s| s <= MAX_INPUT) && method(&extension(name)).is_some()
}

/// Recently changed files first; the first run over everything comes after.
pub fn priority(mtime: OffsetDateTime) -> i16 {
    if OffsetDateTime::now_utc() - mtime < time::Duration::days(7) {
        10
    } else {
        0
    }
}

/// What the workers can read (for the admin view).
#[derive(serde::Serialize, Clone, Copy, Debug, Default)]
pub struct Status {
    pub workers: u32,
    pub pdf: bool,
    pub ocr: bool,
    pub tika: bool,
}

/// The running workers.
pub struct Running {
    pub status: Status,
    stop: tokio::sync::watch::Sender<bool>,
    tasks: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Running {
    /// Stops the workers once their current job is done.
    pub async fn stop(&self) {
        let _ = self.stop.send(true);
        for t in self.tasks.lock().await.drain(..) {
            let _ = t.await;
        }
    }
}

struct Worker {
    st: AppState,
    tools: Tools,
    tika: Option<Tika>,
}

/// Starts the workers (as many as configured; none with `XLRX_EXTRACT_WORKERS=0`).
pub async fn start(st: &AppState) -> Result<(), String> {
    let cfg = &st.cfg;
    if cfg.extract_workers == 0 {
        tracing::info!("Textextraktion ausgeschaltet");
        return Ok(());
    }
    let tools = Tools::find(
        &cfg.pdftotext,
        &cfg.pdftoppm,
        &cfg.tesseract,
        &cfg.ocr_langs,
    )
    .await;
    let tika = cfg.tika_url.clone().map(Tika::new).transpose()?;
    tracing::info!(
        pdf = tools.pdftotext.is_some(),
        ocr = tools.ocr(),
        tika = tika.is_some(),
        "Textextraktion"
    );
    let status = Status {
        workers: cfg.extract_workers,
        pdf: tools.pdftotext.is_some(),
        ocr: tools.ocr(),
        tika: tika.is_some(),
    };
    // Jobs that waited for a program try again; without it they wait again.
    jobs::resume_waiting(&st.db, KIND)
        .await
        .map_err(|e| format!("Aufträge: {e}"))?;
    let worker = Arc::new(Worker {
        st: st.clone(),
        tools,
        tika,
    });
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let tasks = (0..cfg.extract_workers)
        .map(|_| tokio::spawn(work(worker.clone(), stop_rx.clone())))
        .collect();
    st.extract
        .set(Running {
            status,
            stop,
            tasks: tokio::sync::Mutex::new(tasks),
        })
        .map_err(|_| "Die Textextraktion läuft schon".to_string())
}

async fn work(w: Arc<Worker>, mut stop: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *stop.borrow() {
            return;
        }
        match jobs::claim(&w.st.db, &[KIND]).await {
            Ok(Some(job)) => {
                w.handle(&job).await;
                continue;
            }
            Ok(None) => {}
            Err(sqlx::Error::PoolClosed) => return,
            Err(e) => tracing::warn!(error = %e, "Aufträge nicht lesbar"),
        }
        tokio::select! {
            () = w.st.jobs_wake.notified() => {}
            () = tokio::time::sleep(POLL) => {}
            _ = stop.changed() => return,
        }
    }
}

enum Outcome {
    /// Text stored, or nothing (left) to do.
    Done,
    /// Needs a program that is not set up.
    Waiting(&'static str),
}

impl Worker {
    async fn handle(&self, job: &Job) {
        let db = &self.st.db;
        let result = match self.extract(job).await {
            Ok(Outcome::Done) => jobs::finish(db, job.id).await,
            Ok(Outcome::Waiting(reason)) => jobs::park(db, job, reason).await,
            Err(e) => {
                tracing::warn!(key = %job.key, attempt = job.attempts, error = %e, "Textextraktion fehlgeschlagen");
                jobs::retry(db, job, &e).await
            }
        };
        if let Err(e) = result {
            tracing::warn!(key = %job.key, error = %e, "Auftrag nicht abgeschlossen");
        }
    }

    async fn extract(&self, job: &Job) -> Result<Outcome, String> {
        let st = &self.st;
        let hash = content::unhex(&job.key).ok_or("ungültiger Schlüssel")?;
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM content_text WHERE hash = $1)")
                .bind(hash.to_vec())
                .fetch_one(&st.db)
                .await
                .map_err(|e| e.to_string())?;
        if known {
            return Ok(Outcome::Done);
        }
        let data_dir = st.cfg.data_dir.as_deref().ok_or("kein Datenverzeichnis")?;
        // Where the content lies now: any live file with it and a name telling how to read it.
        let nodes: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {NODE_COLS} FROM nodes
              WHERE content_hash = $1 AND deleted_at IS NULL AND kind = 'file'
              ORDER BY id LIMIT 20"
        )))
        .bind(hash.to_vec())
        .fetch_all(&st.db)
        .await
        .map_err(|e| e.to_string())?;
        let mut tried = false;
        for node in nodes {
            let ext = extension(&node.name);
            let Some(method) = method(&ext) else { continue };
            let Some(root) = db::root_by_id(&st.db, node.root_id)
                .await
                .map_err(|e| format!("{e:?}"))?
            else {
                continue;
            };
            let rel = db::rel_path(&st.db, node.id)
                .await
                .map_err(|e| format!("{e:?}"))?;
            let path = roots::dir(data_dir, &root).join(rel);
            tried = true;
            let Some(staged) = content::stage_from(st, &path, &hash)
                .await
                .map_err(|e| format!("{e:?}"))?
            else {
                // Changed or gone since the database saw it; the scan will tell.
                continue;
            };
            return match self.read(method, &ext, &staged).await? {
                Read::Text(source, t) => {
                    text::store(&st.db, &hash, source, &t)
                        .await
                        .map_err(|e| format!("{e:?}"))?;
                    if let Some(s) = st.search.get() {
                        s.wake();
                    }
                    Ok(Outcome::Done)
                }
                Read::Missing(reason) => Ok(Outcome::Waiting(reason)),
            };
        }
        if tried {
            Err("Datei hat sich geändert oder ist nicht lesbar".into())
        } else {
            // No file has this content any more: nothing to do. Should one get it again, the
            // index asks anew.
            Ok(Outcome::Done)
        }
    }

    async fn read(&self, method: Method, ext: &str, staged: &Staged) -> Result<Read, String> {
        let file = staged.path();
        Ok(match method {
            Method::Plain => {
                let mut bytes = Vec::new();
                tokio::fs::File::open(file)
                    .await
                    .map_err(|e| e.to_string())?
                    .take(MAX_PLAIN)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|e| e.to_string())?;
                // Binary content under a text name has no text.
                Read::Text("plain", plain::decode(&bytes).unwrap_or_default())
            }
            Method::Pdf => {
                if self.tools.pdftotext.is_none() {
                    return Ok(Read::Missing("pdftotext nicht installiert"));
                }
                let layer = match self.tools.pdf_text(file).await {
                    Ok(t) => t,
                    Err(Failure::Unreadable(e)) => {
                        tracing::debug!(error = %e, "PDF nicht lesbar");
                        return Ok(Read::Text("pdf", String::new()));
                    }
                    Err(Failure::Failed(e)) => return Err(e),
                };
                // pdftotext ends every page with a form feed.
                let pages = layer.matches('\u{c}').count().max(1);
                if letters(&layer) >= SCAN_BELOW_PER_PAGE * pages {
                    return Ok(Read::Text("pdf", layer));
                }
                if !self.tools.ocr() {
                    return Ok(Read::Missing("Texterkennung (OCR) nicht eingerichtet"));
                }
                let work = OcrDir::new(file)?;
                match self.tools.pdf_ocr(file, &work.0).await {
                    Ok(t) if letters(&t) > letters(&layer) => Read::Text("ocr", t),
                    Ok(_) => Read::Text("pdf", layer),
                    Err(Failure::Unreadable(_)) => Read::Text("pdf", layer),
                    Err(Failure::Failed(e)) => return Err(e),
                }
            }
            Method::Scan => {
                if self.tools.tesseract.is_none() {
                    return Ok(Read::Missing("Texterkennung (OCR) nicht eingerichtet"));
                }
                match self.tools.image_ocr(file).await {
                    Ok(t) => Read::Text("ocr", t),
                    Err(Failure::Unreadable(_)) => Read::Text("ocr", String::new()),
                    Err(Failure::Failed(e)) => return Err(e),
                }
            }
            Method::Tika => {
                let Some(tika) = &self.tika else {
                    return Ok(Read::Missing("Tika nicht eingerichtet"));
                };
                match tika.text(file, ext).await {
                    Ok(t) => Read::Text("tika", t),
                    Err(tika::Error::Rejected(code)) => {
                        tracing::debug!(code, "Tika kann das Dokument nicht lesen");
                        Read::Text("tika", String::new())
                    }
                    Err(e @ tika::Error::Failed(_)) => return Err(e.to_string()),
                }
            }
        })
    }
}

enum Read {
    /// The text (possibly empty: there is none) and how it was obtained.
    Text(&'static str, String),
    /// A program is missing.
    Missing(&'static str),
}

fn letters(text: &str) -> usize {
    text.chars().filter(|c| c.is_alphanumeric()).count()
}

/// A directory for page images next to the staged copy, removed afterwards.
struct OcrDir(std::path::PathBuf);

impl OcrDir {
    fn new(staged: &Path) -> Result<Self, String> {
        let mut name = staged.as_os_str().to_owned();
        name.push(".seiten");
        let dir = std::path::PathBuf::from(name);
        std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Self(dir))
    }
}

impl Drop for OcrDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_read_how() {
        assert_eq!(method("pdf"), Some(Method::Pdf));
        assert_eq!(method("docx"), Some(Method::Tika));
        assert_eq!(method("tiff"), Some(Method::Scan));
        assert_eq!(method("md"), Some(Method::Plain));
        assert_eq!(method("jpg"), None);
        assert!(wanted("Rechnung.PDF", Some(1000)));
        assert!(!wanted("Rechnung.pdf", Some(MAX_INPUT + 1)));
        assert!(!wanted("Rechnung.pdf", None));
        assert!(!wanted("Urlaub.jpg", Some(1000)));
    }
}
