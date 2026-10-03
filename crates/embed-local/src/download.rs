//! Fetches the models once from Hugging Face, at fixed revisions (a model changing upstream must
//! not change the vectors quietly). Files already there are kept.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

/// Repository, revision, file and where it goes below the models directory.
pub const FILES: &[(&str, &str, &str, &str)] = &[
    (
        E5,
        E5_REV,
        "config.json",
        "multilingual-e5-small/config.json",
    ),
    (
        E5,
        E5_REV,
        "tokenizer.json",
        "multilingual-e5-small/tokenizer.json",
    ),
    (
        E5,
        E5_REV,
        "model.safetensors",
        "multilingual-e5-small/model.safetensors",
    ),
    (
        MCLIP,
        MCLIP_REV,
        "config.json",
        "clip-ViT-B-32-multilingual-v1/config.json",
    ),
    (
        MCLIP,
        MCLIP_REV,
        "tokenizer.json",
        "clip-ViT-B-32-multilingual-v1/tokenizer.json",
    ),
    (
        MCLIP,
        MCLIP_REV,
        "model.safetensors",
        "clip-ViT-B-32-multilingual-v1/model.safetensors",
    ),
    (
        MCLIP,
        MCLIP_REV,
        "2_Dense/config.json",
        "clip-ViT-B-32-multilingual-v1/2_Dense/config.json",
    ),
    (
        MCLIP,
        MCLIP_REV,
        "2_Dense/model.safetensors",
        "clip-ViT-B-32-multilingual-v1/2_Dense/model.safetensors",
    ),
    (
        CLIP,
        CLIP_REV,
        "0_CLIPModel/config.json",
        "clip-ViT-B-32/0_CLIPModel/config.json",
    ),
    (
        CLIP,
        CLIP_REV,
        "0_CLIPModel/model.safetensors",
        "clip-ViT-B-32/0_CLIPModel/model.safetensors",
    ),
];

const E5: &str = "intfloat/multilingual-e5-small";
const E5_REV: &str = "614241f622f53c4eeff9890bdc4f31cfecc418b3";
const MCLIP: &str = "sentence-transformers/clip-ViT-B-32-multilingual-v1";
const MCLIP_REV: &str = "58edf8cada9e398793dca955574a48cbb7f18be2";
const CLIP: &str = "sentence-transformers/clip-ViT-B-32";
const CLIP_REV: &str = "327ab6726d33c0e22f920c83f2ff9e4bd38ca37f";

/// Are all files there?
pub fn complete(dir: &Path) -> bool {
    FILES
        .iter()
        .all(|(_, _, _, dest)| dir.join(dest).metadata().is_ok_and(|m| m.len() > 0))
}

/// Downloads what is missing from `base` (`https://huggingface.co` or a mirror).
pub fn download(dir: &Path, base: &str) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .build()
        .into();
    for (repo, rev, file, dest) in FILES {
        let target = dir.join(dest);
        if target.metadata().is_ok_and(|m| m.len() > 0) {
            continue;
        }
        let parent = target.parent().expect("below the models directory");
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        let url = format!("{}/{repo}/resolve/{rev}/{file}", base.trim_end_matches('/'));
        tracing::info!(%url, "Lade Modell");
        let mut res = agent.get(&url).call().map_err(|e| format!("{url}: {e}"))?;
        let part = target.with_extension("part");
        let mut out =
            std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
        let mut reader = res
            .body_mut()
            .with_config()
            .limit(4 * 1024 * 1024 * 1024)
            .reader();
        std::io::copy(&mut reader, &mut out).map_err(|e| format!("{url}: {e}"))?;
        out.flush().map_err(|e| e.to_string())?;
        out.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&part, &target).map_err(|e| format!("{}: {e}", target.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_a_mirror_and_only_once() {
        // A tiny "mirror" answering every path with its own name.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let s = served.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                let body = path.into_bytes();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
                s.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let dir = std::env::temp_dir().join(format!("embed-local-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!complete(&dir));
        download(&dir, &format!("http://{addr}")).unwrap();
        assert!(complete(&dir));
        let e5 =
            std::fs::read_to_string(dir.join("multilingual-e5-small/model.safetensors")).unwrap();
        assert_eq!(e5, format!("/{E5}/resolve/{E5_REV}/model.safetensors"));
        let n = served.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(n, FILES.len());
        // Nothing is fetched again.
        download(&dir, &format!("http://{addr}")).unwrap();
        assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), n);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
