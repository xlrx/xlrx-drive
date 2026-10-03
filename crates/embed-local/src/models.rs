//! The models (PLAN 7.5), on the CPU:
//!
//! - **multilingual-e5-small** (texts): BERT, mean of the token vectors, length 1. Callers add
//!   `query: ` / `passage: ` themselves (xlrx-drive does).
//! - **clip-ViT-B-32-multilingual-v1** (query texts for pictures): a multilingual DistilBERT
//!   trained into the space of CLIP ViT-B/32 – mean of the token vectors, then a linear layer to
//!   512 dimensions.
//! - **CLIP ViT-B/32** (pictures): the vision transformer and its projection, with CLIP's own
//!   preparation (shortest side to 224 bicubic, centre crop, normalized colours).
//!
//! Weights are read into memory (no memory mapping: no `unsafe`), as 32-bit floats.

use std::path::Path;

use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{Linear, VarBuilder};
use candle_transformers::models::{bert, clip, distilbert};
use tokenizers::{Tokenizer, TruncationParams};

pub type Result<T> = std::result::Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn weights(path: &Path) -> Result<VarBuilder<'static>> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    VarBuilder::from_buffered_safetensors(bytes, DType::F32, &Device::Cpu).map_err(err)
}

fn tokenizer(path: &Path, max_len: usize) -> Result<Tokenizer> {
    let mut t = Tokenizer::from_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    t.with_padding(None);
    t.with_truncation(Some(TruncationParams {
        max_length: max_len,
        ..Default::default()
    }))
    .map_err(err)?;
    Ok(t)
}

fn config<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))
}

/// Mean over the tokens (all count: one text at a time, no padding), then length 1.
fn mean_normalized(hidden: &Tensor) -> Result<Tensor> {
    let mean = hidden.mean(1).map_err(err)?;
    clip::div_l2_norm(&mean).map_err(err)
}

fn to_vec(t: &Tensor) -> Result<Vec<f32>> {
    t.squeeze(0).map_err(err)?.to_vec1::<f32>().map_err(err)
}

/// A text model: its vector and how many tokens it read.
pub trait TextModel: Send + Sync {
    fn embed(&self, text: &str) -> Result<(Vec<f32>, usize)>;
}

/// multilingual-e5-small.
pub struct E5 {
    model: bert::BertModel,
    tokenizer: Tokenizer,
}

impl E5 {
    pub fn load(dir: &Path) -> Result<Self> {
        let cfg: bert::Config = config(&dir.join("config.json"))?;
        let model =
            bert::BertModel::load(weights(&dir.join("model.safetensors"))?, &cfg).map_err(err)?;
        Ok(Self {
            model,
            tokenizer: tokenizer(&dir.join("tokenizer.json"), 512)?,
        })
    }
}

impl TextModel for E5 {
    fn embed(&self, text: &str) -> Result<(Vec<f32>, usize)> {
        let enc = self.tokenizer.encode(text, true).map_err(err)?;
        let ids = enc.get_ids();
        let input = Tensor::new(ids, &Device::Cpu)
            .and_then(|t| t.unsqueeze(0))
            .map_err(err)?;
        let types = input.zeros_like().map_err(err)?;
        let mask = input.ones_like().map_err(err)?;
        let hidden = self
            .model
            .forward(&input, &types, Some(&mask))
            .map_err(err)?;
        Ok((to_vec(&mean_normalized(&hidden)?)?, ids.len()))
    }
}

/// The multilingual CLIP text encoder.
pub struct ClipText {
    model: distilbert::DistilBertModel,
    dense: Linear,
    tokenizer: Tokenizer,
}

impl ClipText {
    pub fn load(dir: &Path) -> Result<Self> {
        let cfg: distilbert::Config = config(&dir.join("config.json"))?;
        let model =
            distilbert::DistilBertModel::load(weights(&dir.join("model.safetensors"))?, &cfg)
                .map_err(err)?;
        let dense = candle_nn::linear_no_bias(
            768,
            512,
            weights(&dir.join("2_Dense/model.safetensors"))?.pp("linear"),
        )
        .map_err(err)?;
        Ok(Self {
            model,
            dense,
            tokenizer: tokenizer(&dir.join("tokenizer.json"), 128)?,
        })
    }
}

impl TextModel for ClipText {
    fn embed(&self, text: &str) -> Result<(Vec<f32>, usize)> {
        let enc = self.tokenizer.encode(text, true).map_err(err)?;
        let ids = enc.get_ids();
        let input = Tensor::new(ids, &Device::Cpu)
            .and_then(|t| t.unsqueeze(0))
            .map_err(err)?;
        // DistilBERT's mask marks the positions to leave out: none.
        let mask = Tensor::zeros((1, 1, 1, ids.len()), DType::U8, &Device::Cpu).map_err(err)?;
        let hidden = self.model.forward(&input, &mask).map_err(err)?;
        let mean = hidden.mean(1).map_err(err)?;
        let projected = self.dense.forward(&mean).map_err(err)?;
        let v = clip::div_l2_norm(&projected).map_err(err)?;
        Ok((to_vec(&v)?, ids.len()))
    }
}

/// The CLIP ViT-B/32 picture encoder.
pub struct ClipVision {
    vision: clip::vision_model::ClipVisionTransformer,
    projection: Linear,
}

/// CLIP's picture size and colour statistics.
const SIZE: u32 = 224;
const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const STD: [f32; 3] = [0.268_629_54, 0.261_302_6, 0.275_777_1];

impl ClipVision {
    pub fn load(dir: &Path) -> Result<Self> {
        let cfg = clip::ClipConfig::vit_base_patch32();
        let vb = weights(&dir.join("0_CLIPModel/model.safetensors"))?;
        let vision = clip::vision_model::ClipVisionTransformer::new(
            vb.pp("vision_model"),
            &cfg.vision_config,
        )
        .map_err(err)?;
        let projection = candle_nn::linear_no_bias(
            cfg.vision_config.embed_dim,
            cfg.vision_config.projection_dim,
            vb.pp("visual_projection"),
        )
        .map_err(err)?;
        Ok(Self { vision, projection })
    }

    pub fn embed(&self, picture: &image::DynamicImage) -> Result<Vec<f32>> {
        let pixels = prepare(picture)?;
        let features = self
            .vision
            .forward(&pixels)
            .and_then(|t| self.projection.forward(&t))
            .map_err(err)?;
        to_vec(&clip::div_l2_norm(&features).map_err(err)?)
    }
}

/// CLIP's preparation: shortest side to 224 (bicubic), the centre cut out, colours normalized;
/// a tensor of 1×3×224×224.
pub fn prepare(picture: &image::DynamicImage) -> Result<Tensor> {
    let rgb = picture.to_rgb8();
    let (w, h) = rgb.dimensions();
    if w == 0 || h == 0 {
        return Err("leeres Bild".into());
    }
    let scale = SIZE as f32 / w.min(h) as f32;
    let (nw, nh) = (
        ((w as f32 * scale).round() as u32).max(SIZE),
        ((h as f32 * scale).round() as u32).max(SIZE),
    );
    let resized = image::imageops::resize(&rgb, nw, nh, image::imageops::FilterType::CatmullRom);
    let (x, y) = ((nw - SIZE) / 2, (nh - SIZE) / 2);
    let crop = image::imageops::crop_imm(&resized, x, y, SIZE, SIZE).to_image();
    let n = (SIZE * SIZE) as usize;
    let mut data = vec![0f32; 3 * n];
    for (i, p) in crop.pixels().enumerate() {
        for c in 0..3 {
            data[c * n + i] = (f32::from(p.0[c]) / 255.0 - MEAN[c]) / STD[c];
        }
    }
    Tensor::from_vec(data, (1, 3, SIZE as usize, SIZE as usize), &Device::Cpu).map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cosine similarity of two vectors of length 1.
    fn similarity(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn preparation_like_clip() {
        // A wide red picture: cut to the centre, normalized per channel.
        let img = image::RgbImage::from_pixel(600, 300, image::Rgb([255, 0, 0]));
        let t = prepare(&image::DynamicImage::ImageRgb8(img)).unwrap();
        assert_eq!(t.dims(), &[1, 3, 224, 224]);
        let v = t.flatten_all().unwrap().to_vec1::<f32>().unwrap();
        let n = 224 * 224;
        assert!((v[0] - (1.0 - MEAN[0]) / STD[0]).abs() < 1e-4);
        assert!((v[n] - (0.0 - MEAN[1]) / STD[1]).abs() < 1e-4);
        assert!((v[2 * n + 5] - (0.0 - MEAN[2]) / STD[2]).abs() < 1e-4);
        // Tiny pictures are scaled up.
        let small = image::RgbImage::from_pixel(10, 20, image::Rgb([0, 0, 0]));
        assert_eq!(
            prepare(&image::DynamicImage::ImageRgb8(small))
                .unwrap()
                .dims(),
            &[1, 3, 224, 224]
        );
    }

    /// A small BERT with random weights computes (the matrix kernels run; in CI also without
    /// AVX, under QEMU with a Goldmont processor).
    #[test]
    fn kleines_modell_rechnet() {
        let cfg: bert::Config = serde_json::from_value(serde_json::json!({
            "vocab_size": 300, "hidden_size": 64, "num_hidden_layers": 2,
            "num_attention_heads": 4, "intermediate_size": 128, "hidden_act": "gelu",
            "hidden_dropout_prob": 0.0, "max_position_embeddings": 128, "type_vocab_size": 2,
            "initializer_range": 0.02, "layer_norm_eps": 1e-12, "pad_token_id": 0
        }))
        .unwrap();
        let vars = candle_nn::VarMap::new();
        let vb = VarBuilder::from_varmap(&vars, DType::F32, &Device::Cpu);
        let model = bert::BertModel::load(vb, &cfg).unwrap();
        let ids: Vec<u32> = (1..=60).collect();
        let input = Tensor::new(ids.as_slice(), &Device::Cpu)
            .unwrap()
            .unsqueeze(0)
            .unwrap();
        let hidden = model
            .forward(&input, &input.zeros_like().unwrap(), None)
            .unwrap();
        assert_eq!(hidden.dims(), &[1, 60, 64]);
        let v = to_vec(&mean_normalized(&hidden).unwrap()).unwrap();
        assert!(v.iter().all(|x| x.is_finite()));
        assert!((v.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-4);
        // The same picture preparation and projection path as CLIP.
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            300,
            200,
            image::Rgb([9, 99, 199]),
        ));
        let t = prepare(&img).unwrap();
        let w = Tensor::randn(0f32, 0.02, (16, 3 * 224 * 224), &Device::Cpu).unwrap();
        let out = t.flatten_from(1).unwrap().matmul(&w.t().unwrap()).unwrap();
        assert_eq!(out.dims(), &[1, 16]);
    }

    /// The real models, if they are there (`EMBED_TEST_MODELS=/pfad/zu/models`).
    fn models() -> Option<std::path::PathBuf> {
        std::env::var_os("EMBED_TEST_MODELS").map(Into::into)
    }

    /// German queries find the right photo (`EMBED_TEST_PHOTOS` with dog_beach.jpg,
    /// snow_mountain.jpg, car.jpg, cat.jpg).
    #[test]
    fn clip_findet_fotos_auf_deutsch() {
        let (Some(dir), Some(photos)) = (models(), std::env::var_os("EMBED_TEST_PHOTOS")) else {
            return;
        };
        let photos = std::path::PathBuf::from(photos);
        let text = ClipText::load(&dir.join("clip-ViT-B-32-multilingual-v1")).unwrap();
        let vision = ClipVision::load(&dir.join("clip-ViT-B-32")).unwrap();
        let names = ["dog_beach", "snow_mountain", "car", "cat"];
        let pics: Vec<Vec<f32>> = names
            .iter()
            .map(|n| {
                let bytes = std::fs::read(photos.join(format!("{n}.jpg"))).unwrap();
                vision.embed(&crate::api::decode(&bytes).unwrap()).unwrap()
            })
            .collect();
        let queries = [
            "Hund am Strand",
            "Berge im Schnee",
            "rotes Auto",
            "schlafende Katze",
        ];
        for (i, q) in queries.iter().enumerate() {
            let v = text.embed(q).unwrap().0;
            let sims: Vec<f32> = pics.iter().map(|p| similarity(&v, p)).collect();
            eprintln!("clip „{q}“: {sims:.3?}");
            let best = sims
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap()
                .0;
            assert_eq!(names[best], names[i], "{q}");
        }
    }

    /// Prints how close related and unrelated texts are (to choose the distance limit).
    #[test]
    fn e5_abstaende() {
        let Some(dir) = models() else { return };
        if std::env::var_os("EMBED_CALIBRATE").is_none() {
            return;
        }
        let m = E5::load(&dir.join("multilingual-e5-small")).unwrap();
        let docs = [
            "Rechnung der Firma Müller über die Wartung der Heizung im November. Betrag 238 Euro.",
            "Packliste für den Urlaub am Strand: Sonnencreme, Handtuch, Badehose und ein Buch.",
            "Befund der Blutwerte vom Hausarzt: Cholesterin leicht erhöht, Kontrolle in drei Monaten.",
            "Mietvertrag für die Wohnung in der Lindenstraße, Kündigungsfrist drei Monate.",
            "Rezept für Apfelkuchen: 500 g Mehl, 200 g Zucker, vier Äpfel, Zimt.",
            "Protokoll der Elternversammlung der Klasse 4b vom 12. März.",
        ];
        let queries = [
            ("Heizungswartung Kosten", 0),
            ("Ferien am Meer", 1),
            ("Laborwerte Arzt", 2),
            ("Wohnung kündigen", 3),
            ("Kuchen backen", 4),
            ("Schule Elternabend", 5),
            ("Steuererklärung 2024", usize::MAX),
        ];
        let d: Vec<Vec<f32>> = docs
            .iter()
            .map(|t| m.embed(&format!("passage: {t}")).unwrap().0)
            .collect();
        for (q, want) in queries {
            let v = m.embed(&format!("query: {q}")).unwrap().0;
            let sims: Vec<f32> = d.iter().map(|x| similarity(&v, x)).collect();
            eprintln!("e5 „{q}“ (soll {want}): {sims:.3?}");
        }
    }

    #[test]
    fn e5_versteht_deutsch() {
        let Some(dir) = models() else { return };
        let m = E5::load(&dir.join("multilingual-e5-small")).unwrap();
        let e = |t: &str| m.embed(t).unwrap().0;
        let q = e("query: Rechnung Heizung");
        let invoice =
            e("passage: Rechnung der Firma Müller über die Wartung der Heizung im November.");
        let holiday = e("passage: Packliste für den Urlaub am Strand: Sonnencreme und Handtuch.");
        let (a, b) = (similarity(&q, &invoice), similarity(&q, &holiday));
        eprintln!("e5: Rechnung {a:.3}, Urlaub {b:.3}");
        assert!(a > b + 0.05);
        assert_eq!(q.len(), 384);
        assert!((similarity(&q, &q) - 1.0).abs() < 1e-4);
    }
}
