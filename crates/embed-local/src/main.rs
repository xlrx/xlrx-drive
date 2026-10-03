//! `embed-local`: embeddings in the home network for xlrx-drive (PLAN 7.5), so folders marked
//! "Nur lokal" can be searched by meaning and by what their pictures show without a byte
//! leaving the house. Speaks the OpenAI API (`POST /v1/embeddings`).
//!
//! ```text
//! embed-local             serve (EMBED_BIND, default 0.0.0.0:8090)
//! embed-local download    fetch the models into EMBED_MODELS_DIR (default /models)
//! embed-local bench       time texts and pictures on this machine
//! ```
//!
//! Settings: `EMBED_MODELS_DIR`, `EMBED_BIND`, `EMBED_DOWNLOAD=1` (fetch missing models before
//! serving), `EMBED_CLIP=0` (no picture model), `RAYON_NUM_THREADS` (cores used, e.g. 2 on the
//! NAS by day).

mod api;
mod download;
mod models;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use models::TextModel;

pub const TEXT_MODEL: &str = "multilingual-e5-small";
pub const CLIP_MODEL: &str = "clip-ViT-B-32-multilingual-v1";

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn models_dir() -> PathBuf {
    var("EMBED_MODELS_DIR").map_or_else(|| PathBuf::from("/models"), PathBuf::from)
}

fn hub() -> String {
    var("EMBED_HUB_URL").unwrap_or_else(|| "https://huggingface.co".into())
}

/// What the processor offers (the matrix kernels choose by it at run time).
fn cpu() -> String {
    #[cfg(target_arch = "x86_64")]
    {
        format!(
            "SSE4.2 {}, AVX {}, AVX2 {}, FMA {}",
            std::is_x86_feature_detected!("sse4.2"),
            std::is_x86_feature_detected!("avx"),
            std::is_x86_feature_detected!("avx2"),
            std::is_x86_feature_detected!("fma")
        )
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        std::env::consts::ARCH.to_string()
    }
}

fn load() -> Result<api::Models, String> {
    tracing::info!(cpu = %cpu(), "Prozessor");
    let dir = models_dir();
    let started = Instant::now();
    let text = models::E5::load(&dir.join(TEXT_MODEL))?;
    let clip = if var("EMBED_CLIP").is_some_and(|v| v == "0" || v == "off") {
        None
    } else {
        Some((
            CLIP_MODEL.to_string(),
            models::ClipText::load(&dir.join(CLIP_MODEL))?,
            models::ClipVision::load(&dir.join("clip-ViT-B-32"))?,
        ))
    };
    tracing::info!(
        seconds = started.elapsed().as_secs_f32(),
        clip = clip.is_some(),
        "Modelle geladen"
    );
    Ok(api::Models {
        text: Some((TEXT_MODEL.to_string(), Box::new(text))),
        clip,
        busy: tokio::sync::Semaphore::new(1),
    })
}

fn bench() -> Result<(), String> {
    let m = load()?;
    println!("CPU: {}", cpu());
    let piece = "passage: ".to_string()
        + &"Die Rechnung für die Wartung der Heizung ist am Montag gekommen und muss bis Ende des Monats bezahlt werden. "
            .repeat(16);
    let (_, text) = m.text.as_ref().expect("loaded");
    let t = Instant::now();
    let (_, tokens) = text.embed(&piece)?;
    let n = 5;
    for _ in 0..n {
        text.embed(&piece)?;
    }
    println!(
        "Text ({tokens} Tokens): {:.0} ms je Stück",
        t.elapsed().as_secs_f64() * 1000.0 / f64::from(n + 1)
    );
    let t = Instant::now();
    text.embed("query: Rechnung Heizung")?;
    println!("Suchanfrage: {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    if let Some((_, ct, cv)) = &m.clip {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(1024, 768, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        }));
        let t = Instant::now();
        for _ in 0..n {
            cv.embed(&img)?;
        }
        println!(
            "Bild: {:.0} ms je Bild",
            t.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
        );
        let t = Instant::now();
        ct.embed("Hund am Strand")?;
        println!(
            "Bildsuche (Text): {:.0} ms",
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(())
}

async fn serve() -> Result<(), String> {
    let dir = models_dir();
    if !download::complete(&dir) {
        if var("EMBED_DOWNLOAD").is_some_and(|v| v == "1" || v == "true") {
            let d = dir.clone();
            tokio::task::spawn_blocking(move || download::download(&d, &hub()))
                .await
                .map_err(|e| e.to_string())??;
        } else {
            return Err(format!(
                "Modelle fehlen in {} (embed-local download, oder EMBED_DOWNLOAD=1)",
                dir.display()
            ));
        }
    }
    let models = tokio::task::spawn_blocking(load)
        .await
        .map_err(|e| e.to_string())??;
    let bind = var("EMBED_BIND").unwrap_or_else(|| "0.0.0.0:8090".into());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| format!("{bind}: {e}"))?;
    tracing::info!(%bind, "embed-local bereit");
    axum::serve(listener, api::router(Arc::new(models)))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let result = match std::env::args().nth(1).as_deref() {
        None | Some("serve") => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
            .and_then(|rt| rt.block_on(serve())),
        Some("download") => download::download(&models_dir(), &hub()),
        Some("bench") => bench(),
        Some(other) => Err(format!(
            "Unbekannter Befehl „{other}“ (serve, download, bench)"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("embed-local: {e}");
            ExitCode::FAILURE
        }
    }
}
