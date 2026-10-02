//! Thumbnails of images: made on first request from the file's bytes, kept in the state directory
//! as `store/thumbs/ab/cd/<hash>-<size>.jpg|png`.
//!
//! The key is the content hash of exactly the bytes the thumbnail was made from, never the hash
//! the database remembers. So a thumbnail always shows the content it is filed under, even if the
//! file changed after the last scan – no one ever sees a picture of someone else's file.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};

/// Sizes the web app asks for (longest side in pixels).
pub const SIZES: [u32; 3] = [64, 256, 1024];
/// Larger files get no thumbnail (they would take too long on the NAS).
pub const MAX_INPUT: u64 = 64 * 1024 * 1024;
pub const THUMBS: &str = "store/thumbs";

/// Images we make thumbnails of (by type). SVG is not among them: it could contain scripts.
pub fn supported(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/bmp" | "image/tiff"
    )
}

fn base(state_dir: &Path, hash: &[u8; 32], size: u32) -> PathBuf {
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    state_dir
        .join(THUMBS)
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(format!("{hex}-{size}"))
}

/// What is known about the thumbnail of some content.
pub enum Cached {
    Image(PathBuf, &'static str),
    /// Made before and failed (not an image after all, damaged, too large): not tried again.
    Failed,
    Missing,
}

pub fn cached(state_dir: &Path, hash: &[u8; 32], size: u32) -> Cached {
    let b = base(state_dir, hash, size);
    for (ext, mime) in [("jpg", "image/jpeg"), ("png", "image/png")] {
        let p = b.with_extension(ext);
        if p.is_file() {
            return Cached::Image(p, mime);
        }
    }
    if b.with_extension("failed").is_file() {
        Cached::Failed
    } else {
        Cached::Missing
    }
}

/// Scales `bytes` down to fit `size`×`size` (smaller pictures stay as they are), upright (EXIF
/// orientation applied). JPEG, or PNG when the image has transparency.
pub fn render(bytes: &[u8], size: u32) -> Result<(Vec<u8>, &'static str), String> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    // Protection against "decompression bombs": a small file that claims a huge picture.
    let mut limits = Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    let thumb = if img.width() <= size && img.height() <= size {
        img
    } else {
        img.thumbnail(size, size)
    };
    let mut out = Vec::new();
    if thumb.color().has_alpha() {
        thumb
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        Ok((out, "image/png"))
    } else {
        let rgb = DynamicImage::ImageRgb8(thumb.to_rgb8());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82)
            .encode_image(&rgb)
            .map_err(|e| e.to_string())?;
        Ok((out, "image/jpeg"))
    }
}

/// Stores a thumbnail (or the failure) under its key; a new file only appears complete.
pub fn store(
    state_dir: &Path,
    hash: &[u8; 32],
    size: u32,
    made: Option<(&[u8], &'static str)>,
) -> std::io::Result<()> {
    let b = base(state_dir, hash, size);
    let (bytes, ext): (&[u8], _) = match made {
        Some((bytes, "image/png")) => (bytes, "png"),
        Some((bytes, _)) => (bytes, "jpg"),
        None => (b"", "failed"),
    };
    let dir = b.parent().expect("has a parent");
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(super::store::temp_name());
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, b.with_extension(ext))
}

/// Thumbnails whose content no file or version has any more.
pub fn hashes_on_disk(state_dir: &Path) -> Vec<([u8; 32], PathBuf)> {
    let mut out = Vec::new();
    let Ok(level1) = std::fs::read_dir(state_dir.join(THUMBS)) else {
        return out;
    };
    for d1 in level1.flatten() {
        let Ok(level2) = std::fs::read_dir(d1.path()) else {
            continue;
        };
        for d2 in level2.flatten() {
            let Ok(files) = std::fs::read_dir(d2.path()) else {
                continue;
            };
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                let hex = name.get(..64).unwrap_or_default();
                let mut hash = [0u8; 32];
                let ok = hex.len() == 64
                    && hash.iter_mut().enumerate().all(|(i, b)| {
                        u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                            .map(|v| *b = v)
                            .is_ok()
                    });
                if ok {
                    out.push((hash, f.path()));
                }
            }
        }
    }
    out
}

/// Removes thumbnails of content that no file (live or in the trash) or version has any more.
pub async fn cleanup(st: &crate::state::AppState) -> crate::error::ApiResult<()> {
    let Some(state_dir) = st.cfg.state_dir.clone() else {
        return Ok(());
    };
    let dir = state_dir.clone();
    let on_disk = tokio::task::spawn_blocking(move || hashes_on_disk(&dir))
        .await
        .map_err(|e| crate::error::ApiError::Internal(e.to_string()))?;
    for batch in on_disk.chunks(1000) {
        let hashes: Vec<Vec<u8>> = batch.iter().map(|(h, _)| h.to_vec()).collect();
        let used: std::collections::HashSet<Vec<u8>> = sqlx::query_scalar(
            "SELECT content_hash FROM nodes WHERE content_hash = ANY($1)
             UNION SELECT content_hash FROM versions WHERE content_hash = ANY($1)",
        )
        .bind(&hashes)
        .fetch_all(&st.db)
        .await?
        .into_iter()
        .collect();
        for (hash, path) in batch {
            if !used.contains(hash.as_slice()) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    fn png(img: DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn fits_the_box_and_keeps_the_aspect_ratio() {
        let src = png(DynamicImage::ImageRgb8(RgbImage::from_pixel(
            800,
            400,
            Rgb([200, 30, 30]),
        )));
        let (bytes, mime) = render(&src, 256).unwrap();
        assert_eq!(mime, "image/jpeg");
        let t = image::load_from_memory(&bytes).unwrap();
        assert_eq!((t.width(), t.height()), (256, 128));
    }

    #[test]
    fn small_pictures_are_not_enlarged() {
        let src = png(DynamicImage::ImageRgb8(RgbImage::new(8, 6)));
        let (bytes, _) = render(&src, 256).unwrap();
        let t = image::load_from_memory(&bytes).unwrap();
        assert_eq!((t.width(), t.height()), (8, 6));
    }

    #[test]
    fn transparency_stays_png() {
        let src = png(DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            50,
            100,
            Rgba([0, 0, 0, 0]),
        )));
        let (bytes, mime) = render(&src, 64).unwrap();
        assert_eq!(mime, "image/png");
        let t = image::load_from_memory(&bytes).unwrap();
        assert_eq!((t.width(), t.height()), (32, 64));
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn refuses_garbage_and_bombs() {
        assert!(render(b"kein Bild", 64).is_err());
        // A valid PNG header that claims 30000 × 30000 pixels (over the limit).
        let mut bomb = png(DynamicImage::ImageRgb8(RgbImage::new(1, 1)));
        bomb[16..20].copy_from_slice(&30_000u32.to_be_bytes());
        bomb[20..24].copy_from_slice(&30_000u32.to_be_bytes());
        let crc = crc32(&bomb[12..29]);
        bomb[29..33].copy_from_slice(&crc.to_be_bytes());
        let err = render(&bomb, 64).unwrap_err();
        assert!(err.to_lowercase().contains("limit"), "{err}");
    }

    #[test]
    fn only_raster_images() {
        assert!(supported("image/jpeg") && supported("image/png"));
        // SVG could contain scripts; everything else is no picture.
        assert!(!supported("image/svg+xml"));
        assert!(!supported("text/plain"));
        assert!(!supported("application/pdf"));
    }

    #[test]
    fn stored_and_found() {
        let dir = tempfile::tempdir().unwrap();
        let h = [7u8; 32];
        assert!(matches!(cached(dir.path(), &h, 64), Cached::Missing));
        store(dir.path(), &h, 64, Some((b"x", "image/png"))).unwrap();
        assert!(matches!(
            cached(dir.path(), &h, 64),
            Cached::Image(_, "image/png")
        ));
        assert!(matches!(cached(dir.path(), &h, 256), Cached::Missing));
        store(dir.path(), &h, 256, None).unwrap();
        assert!(matches!(cached(dir.path(), &h, 256), Cached::Failed));
        assert_eq!(hashes_on_disk(dir.path()).len(), 2);
    }
}
