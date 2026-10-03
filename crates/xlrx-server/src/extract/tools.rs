//! Programs on the NAS: `pdftotext` (text layer of PDFs), `pdftoppm` + `tesseract` (OCR of
//! scans). Each runs with a time limit and a cap on its output; tesseract with one thread, so the
//! NAS stays responsive.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Most text bytes taken from a program.
pub const MAX_OUTPUT: usize = 8 * 1024 * 1024;
/// Pages of a scanned PDF that are recognised (the rest is rarely what people search for, and
/// each page takes seconds on the NAS).
pub const MAX_OCR_PAGES: u32 = 30;
const TEXT_TIME: Duration = Duration::from_secs(5 * 60);
const RENDER_TIME: Duration = Duration::from_secs(10 * 60);
const OCR_PAGE_TIME: Duration = Duration::from_secs(5 * 60);

#[derive(Debug)]
pub enum Failure {
    /// The program cannot read this file (damaged, encrypted): trying again will not help.
    Unreadable(String),
    /// Something else went wrong (time limit, program missing): worth another try later.
    Failed(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Unreadable(m) | Failure::Failed(m) => f.write_str(m),
        }
    }
}

/// Which programs are there.
#[derive(Clone, Debug, Default)]
pub struct Tools {
    pub pdftotext: Option<PathBuf>,
    pub pdftoppm: Option<PathBuf>,
    pub tesseract: Option<PathBuf>,
    /// Tesseract languages, e.g. "deu+eng".
    pub ocr_langs: String,
}

impl Tools {
    /// Looks for the configured programs (a missing one only disables what needs it).
    pub async fn find(pdftotext: &Path, pdftoppm: &Path, tesseract: &Path, langs: &str) -> Self {
        let tesseract = match works(tesseract, "--version").await {
            Some(t) if has_languages(&t, langs).await => Some(t),
            Some(_) => {
                tracing::warn!(langs, "Tesseract ohne die Sprachen für die Texterkennung");
                None
            }
            None => None,
        };
        Self {
            pdftotext: works(pdftotext, "-v").await,
            pdftoppm: works(pdftoppm, "-v").await,
            tesseract,
            ocr_langs: langs.to_owned(),
        }
    }

    /// Scans can be recognised.
    pub fn ocr(&self) -> bool {
        self.pdftoppm.is_some() && self.tesseract.is_some()
    }

    /// The text layer of a PDF.
    pub async fn pdf_text(&self, file: &Path) -> Result<String, Failure> {
        let cmd = self
            .pdftotext
            .as_deref()
            .ok_or_else(|| Failure::Failed("pdftotext fehlt".into()))?;
        let args = [
            OsStr::new("-enc"),
            OsStr::new("UTF-8"),
            OsStr::new("-q"),
            file.as_os_str(),
            OsStr::new("-"),
        ];
        match run(cmd, &args, TEXT_TIME).await {
            Ok(out) => Ok(String::from_utf8_lossy(&out).into_owned()),
            // 1: cannot open the PDF, 3: encrypted / no permission.
            Err(Run::Exit(code @ (1 | 3), msg)) => {
                Err(Failure::Unreadable(format!("pdftotext {code}: {msg}")))
            }
            Err(e) => Err(Failure::Failed(format!("pdftotext: {e}"))),
        }
    }

    /// Recognises the first pages of a scanned PDF. `work` is an empty directory for the page
    /// images.
    pub async fn pdf_ocr(&self, file: &Path, work: &Path) -> Result<String, Failure> {
        let (Some(render), Some(_)) = (&self.pdftoppm, &self.tesseract) else {
            return Err(Failure::Failed("Texterkennung nicht eingerichtet".into()));
        };
        let last = MAX_OCR_PAGES.to_string();
        let prefix = work.join("seite");
        let args = [
            OsStr::new("-r"),
            OsStr::new("300"),
            OsStr::new("-gray"),
            OsStr::new("-png"),
            OsStr::new("-f"),
            OsStr::new("1"),
            OsStr::new("-l"),
            OsStr::new(&last),
            file.as_os_str(),
            prefix.as_os_str(),
        ];
        match run(render, &args, RENDER_TIME).await {
            Ok(_) => {}
            Err(Run::Exit(code @ (1 | 3), msg)) => {
                return Err(Failure::Unreadable(format!("pdftoppm {code}: {msg}")));
            }
            Err(e) => return Err(Failure::Failed(format!("pdftoppm: {e}"))),
        }
        let mut pages: Vec<PathBuf> = std::fs::read_dir(work)
            .map_err(|e| Failure::Failed(e.to_string()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "png"))
            .collect();
        // Page numbers are zero-padded to the same width: name order is page order.
        pages.sort();
        let mut text = String::new();
        for page in pages {
            text.push_str(&self.image_ocr(&page).await?);
            text.push('\n');
        }
        Ok(text)
    }

    /// Recognises the text in an image (TIFF scans, rendered PDF pages).
    pub async fn image_ocr(&self, file: &Path) -> Result<String, Failure> {
        let cmd = self
            .tesseract
            .as_deref()
            .ok_or_else(|| Failure::Failed("Tesseract fehlt".into()))?;
        let args = [
            file.as_os_str(),
            OsStr::new("stdout"),
            OsStr::new("-l"),
            OsStr::new(&self.ocr_langs),
        ];
        match run(cmd, &args, OCR_PAGE_TIME).await {
            Ok(out) => Ok(String::from_utf8_lossy(&out).into_owned()),
            Err(e) => Err(Failure::Failed(format!("tesseract: {e}"))),
        }
    }
}

/// The program if it starts (`arg` asks for its version).
async fn works(cmd: &Path, arg: &str) -> Option<PathBuf> {
    let ok = Command::new(cmd)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .is_ok_and(|s| s.success());
    ok.then(|| cmd.to_path_buf())
}

async fn has_languages(tesseract: &Path, langs: &str) -> bool {
    let Ok(Ok(out)) = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(tesseract)
            .arg("--list-langs")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    else {
        return false;
    };
    // Older versions print the list on stderr.
    let listed = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    langs
        .split('+')
        .all(|l| listed.lines().any(|x| x.trim() == l))
}

#[derive(Debug)]
enum Run {
    Exit(i32, String),
    Other(String),
}

impl std::fmt::Display for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Run::Exit(code, msg) => write!(f, "Ende mit {code}: {msg}"),
            Run::Other(m) => f.write_str(m),
        }
    }
}

/// Runs a program and returns what it wrote (at most [`MAX_OUTPUT`] bytes).
async fn run(cmd: &Path, args: &[&OsStr], limit: Duration) -> Result<Vec<u8>, Run> {
    let mut child = Command::new(cmd)
        .args(args)
        .env("OMP_THREAD_LIMIT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Run::Other(format!("{}: {e}", cmd.display())))?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let work = async {
        let read_out = async move {
            let mut out = Vec::new();
            let r = (&mut stdout)
                .take(MAX_OUTPUT as u64)
                .read_to_end(&mut out)
                .await;
            // Closing the pipe at the cap ends a program that would write more (instead of
            // leaving it blocked until the time limit).
            drop(stdout);
            r.map(|_| out)
        };
        let read_err = async move {
            let mut err = Vec::new();
            let r = (&mut stderr).take(64 * 1024).read_to_end(&mut err).await;
            drop(stderr);
            r.map(|_| err)
        };
        let (out, err) = tokio::join!(read_out, read_err);
        let (out, err) = (
            out.map_err(|e| Run::Other(e.to_string()))?,
            err.map_err(|e| Run::Other(e.to_string()))?,
        );
        let status = child.wait().await.map_err(|e| Run::Other(e.to_string()))?;
        Ok::<_, Run>((status, out, err))
    };
    let (status, out, err) = tokio::time::timeout(limit, work)
        .await
        .map_err(|_| Run::Other(format!("Zeitlimit von {} s überschritten", limit.as_secs())))??;
    // Cut off at the cap: what was read is what we wanted.
    if status.success() || out.len() >= MAX_OUTPUT {
        return Ok(out);
    }
    let msg = String::from_utf8_lossy(&err)
        .trim()
        .chars()
        .take(300)
        .collect();
    Err(match status.code() {
        Some(code) => Run::Exit(code, msg),
        None => Run::Other(format!("abgebrochen: {msg}")),
    })
}
