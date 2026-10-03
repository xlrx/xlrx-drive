//! Pictures for the AI (PLAN 7.2, 7.5): read from a copy checked against the content hash,
//! scaled down to at most 1024 pixels and encoded anew – so neither EXIF nor GPS data nor the
//! file name ever leave the NAS with them.

use crate::files::content;
use crate::files::db::{self, NODE_COLS, NodeRow};
use crate::files::roots;
use crate::files::thumbs;
use crate::state::AppState;

use super::provider::Picture;

/// Names of pictures the AI looks at (what the NAS can decode).
pub const NAME_PATTERN: &str = r"\.(jpe?g|png|gif|webp|bmp|tiff?)$";
/// Smaller files are icons and buttons, not photos.
pub const MIN_BYTES: i64 = 10_000;
/// Pictures with a longer side below this are skipped as well.
const MIN_SIDE: u32 = 200;
/// Longest side sent.
pub const SEND_SIZE: u32 = 1024;

pub fn is_image(name: &str) -> bool {
    let ext = crate::search::schema::extension(name);
    matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff"
    )
}

/// A picture of a content, if one can be had.
pub enum Read {
    Picture(Picture),
    /// Not decodable or too small: nothing to analyse.
    Unusable,
    /// No file has it (any more).
    Gone,
}

/// Reads the picture of a content from any live file having it.
pub async fn read(st: &AppState, hash: &[u8; 32]) -> Result<Read, String> {
    let data_dir = st.cfg.data_dir.as_deref().ok_or("kein Datenverzeichnis")?;
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
        if !is_image(&node.name) || node.size.is_none_or(|s| s as u64 > thumbs::MAX_INPUT) {
            continue;
        }
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
        let Some(staged) = content::stage_from(st, &path, hash)
            .await
            .map_err(|e| format!("{e:?}"))?
        else {
            // Changed or gone since the database saw it; the scan will tell.
            continue;
        };
        let _permit = st.thumbnails.acquire().await.map_err(|e| e.to_string())?;
        let file = staged.path().to_path_buf();
        let made = tokio::task::spawn_blocking(move || -> Result<Option<Picture>, String> {
            let bytes = std::fs::read(&file).map_err(|e| e.to_string())?;
            let Ok((out, mime)) = thumbs::render(&bytes, SEND_SIZE) else {
                return Ok(None);
            };
            let (w, h) = image::ImageReader::new(std::io::Cursor::new(&out))
                .with_guessed_format()
                .map_err(|e| e.to_string())?
                .into_dimensions()
                .map_err(|e| e.to_string())?;
            Ok((w.max(h) >= MIN_SIDE).then_some(Picture { bytes: out, mime }))
        })
        .await
        .map_err(|e| e.to_string())??;
        drop(staged);
        return Ok(match made {
            Some(p) => Read::Picture(p),
            None => Read::Unusable,
        });
    }
    if tried {
        Err("Datei hat sich geändert oder ist nicht lesbar".into())
    } else {
        Ok(Read::Gone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_by_name() {
        assert!(is_image("Urlaub.JPG") && is_image("scan.tiff") && is_image("a.webp"));
        assert!(!is_image("Rechnung.pdf") && !is_image("logo.svg") && !is_image("IMG.HEIC"));
        let re = |n: &str| {
            // The SQL pattern says the same (Postgres `~*` behaves like this case-insensitive match).
            let n = n.to_lowercase();
            ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff"]
                .iter()
                .any(|e| n.ends_with(&format!(".{e}")))
        };
        for n in ["a.jpg", "b.JPEG", "c.Tif", "d.pdf", "e.svg", "f.jpg.txt"] {
            assert_eq!(is_image(n), re(n), "{n}");
        }
    }
}
