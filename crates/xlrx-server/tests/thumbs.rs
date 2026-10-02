//! Thumbnails: size and type, cache, damaged images, other people, files changed after the scan,
//! cleanup.

mod common;

use std::io::Cursor;

use common::files::*;
use common::*;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use serde_json::json;
use xlrx_chunk::Chunker;

macro_rules! env_or_skip {
    () => {
        match Env::with_data().await {
            Some(e) => e,
            None => return,
        }
    };
}

fn encode(img: DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), format).unwrap();
    out
}

fn jpeg(w: u32, h: u32, color: [u8; 3]) -> Vec<u8> {
    encode(
        DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, Rgb(color))),
        ImageFormat::Jpeg,
    )
}

fn hex(bytes: &[u8]) -> String {
    let h = Chunker::new().digest_slice(bytes).content.hash.0;
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn thumb_file(env: &Env, hash: &str, size: u32, ext: &str) -> std::path::PathBuf {
    env.data_dir().join(format!(
        "xlrx-state/store/thumbs/{}/{}/{hash}-{size}.{ext}",
        &hash[..2],
        &hash[2..4]
    ))
}

#[tokio::test]
async fn vorschaubilder() {
    let env = env_or_skip!();
    let (mut c, root_id, root_node, dir) = signed_in(&env, "anna").await;
    let photo = jpeg(800, 400, [200, 30, 30]);
    write(&dir.join("Foto.jpg"), &photo);
    write(
        &dir.join("Logo.png"),
        &encode(
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(50, 100, Rgba([0, 0, 0, 0]))),
            ImageFormat::Png,
        ),
    );
    write(&dir.join("Kaputt.jpg"), b"kein JPEG");
    write(&dir.join("Notiz.txt"), b"Text");
    c.post(&format!("/api/roots/{root_id}/scan"), json!({}))
        .await
        .ok();
    let list = c
        .get(&format!("/api/nodes/{root_node}/children"))
        .await
        .ok()
        .clone();
    let foto = find(&list, "Foto.jpg").clone();
    let url = |n: &serde_json::Value, s: u32| {
        format!("/api/nodes/{}/thumbnail?s={s}&v={}", n["id"], n["rev"])
    };

    // Fits the box, keeps the aspect ratio; the browser may keep it for this revision.
    let r = c.get_raw(&url(&foto, 256), &[]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), "image/jpeg");
    assert_eq!(
        r.header("cache-control"),
        "private, max-age=31536000, immutable"
    );
    let t = image::load_from_memory(&r.bytes).unwrap();
    assert_eq!((t.width(), t.height()), (256, 128));
    assert!(thumb_file(&env, &hex(&photo), 256, "jpg").is_file());
    // Without the current revision: only with a check.
    let r = c
        .get_raw(&format!("/api/nodes/{}/thumbnail?s=256", foto["id"]), &[])
        .await;
    assert_eq!(r.header("cache-control"), "private, no-cache");
    // Downloads stay uncached.
    let r = c
        .get_raw(&format!("/api/nodes/{}/content", foto["id"]), &[])
        .await;
    assert_eq!(r.header("cache-control"), "no-store");

    // Transparency stays PNG.
    let logo = find(&list, "Logo.png").clone();
    let r = c.get_raw(&url(&logo, 64), &[]).await;
    assert_eq!(r.header("content-type"), "image/png");
    let t = image::load_from_memory(&r.bytes).unwrap();
    assert_eq!((t.width(), t.height()), (32, 64));

    // No thumbnail: damaged image (remembered, not tried again), text, folder, odd size.
    let kaputt = find(&list, "Kaputt.jpg").clone();
    assert_eq!(c.get_raw(&url(&kaputt, 64), &[]).await.status, 404);
    assert!(thumb_file(&env, &hex(b"kein JPEG"), 64, "failed").is_file());
    assert_eq!(c.get_raw(&url(&kaputt, 64), &[]).await.status, 404);
    assert_eq!(
        c.get_raw(&url(find(&list, "Notiz.txt"), 64), &[])
            .await
            .status,
        404
    );
    assert_eq!(
        c.get_raw(&format!("/api/nodes/{root_node}/thumbnail?s=64"), &[])
            .await
            .status,
        404
    );
    assert_eq!(c.get_raw(&url(&foto, 100), &[]).await.status, 400);

    // Someone else gets nothing, even though the thumbnail exists.
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    assert_eq!(bert.get_raw(&url(&foto, 256), &[]).await.status, 404);

    // Changed on disk after the scan: the thumbnail shows the new content and is filed under its
    // hash – never under the hash the database still remembers.
    let neu = jpeg(300, 600, [20, 20, 220]);
    write(&dir.join("Foto.jpg"), &neu);
    let r = c.get_raw(&url(&foto, 64), &[]).await;
    let t = image::load_from_memory(&r.bytes).unwrap();
    assert_eq!((t.width(), t.height()), (32, 64));
    assert!(thumb_file(&env, &hex(&neu), 64, "jpg").is_file());
    assert!(!thumb_file(&env, &hex(&photo), 64, "jpg").exists());

    // Cleanup: thumbnails of content nobody has any more go, the others stay.
    let orphan = "ab".repeat(32);
    let path = thumb_file(&env, &orphan, 64, "jpg");
    write(&path, b"alt");
    xlrx_server::files::thumbs::cleanup(&env.state)
        .await
        .unwrap();
    assert!(!path.exists());
    assert!(
        thumb_file(&env, &hex(&photo), 256, "jpg").is_file(),
        "Inhalt laut Datenbank noch da"
    );
    env.finish().await;
}
