//! Configuration from environment variables (see `deploy/docker-compose.yml`).

use std::net::SocketAddr;
use std::path::PathBuf;

use base64::Engine as _;
use url::Url;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub bind: SocketAddr,
    /// Public address, e.g. `https://drive.example.de`. Determines the cookie `Secure` flag, the
    /// allowed origin and the passkey origin.
    pub public_url: Url,
    /// Further allowed origins, e.g. the LAN endpoint `https://drive-lan.example.de` (PLAN 5.9).
    pub extra_origins: Vec<Url>,
    /// Relying party ID for passkeys (domain). Default: host of the public address. Set it once: a
    /// later change renders all registered passkeys unusable.
    pub rp_id: String,
    /// Key for secrets in the DB (TOTP). Comes from a Docker secret, never from the DB.
    pub secret_key: [u8; 32],
    /// Built web app (SvelteKit, static). Without it: API only.
    pub web_dir: Option<PathBuf>,
    /// Evaluate `X-Forwarded-For` from the upstream proxy (Caddy).
    pub trust_proxy: bool,
    pub argon2: ArgonParams,
    /// Additional list of forbidden passwords (one line per password).
    pub password_blocklist: Option<PathBuf>,
    /// The NAS volume with the users' files (`/volume1`, mounted as one; PLAN 4.1). Without it, the
    /// server only offers accounts and sign-in.
    pub data_dir: Option<PathBuf>,
    /// Versions, trash and staging (`xlrx-state`). Must be on the same file system as the data so
    /// that files can be reflinked or moved. Default: `<data_dir>/xlrx-state`.
    pub state_dir: Option<PathBuf>,
    /// Location of a person's "My Drive" below `data_dir`; `{user}` is the user name.
    pub home_pattern: String,
    /// Tests only: always copy between the roots and the state directory instead of renaming, as
    /// across Btrfs subvolumes on the NAS.
    pub force_copy: bool,
    /// Watch roots for changes from outside (inotify). On by default; `XLRX_WATCH=0` turns it off
    /// (then only the hourly scan and "Neu einlesen" pick them up).
    pub watch: bool,
    /// Devices (Mac, iPhone) need a second factor again after this many days
    /// (`XLRX_DEVICE_CONFIRM_DAYS`, default 30).
    pub device_confirm_days: u32,
    /// Apache Tika for Office, iWork and mail (`XLRX_TIKA_URL`, e.g. `http://tika:9998`). Only
    /// addresses in the home network are accepted: documents never leave the NAS.
    pub tika_url: Option<Url>,
    /// Programs for PDFs and OCR (`XLRX_PDFTOTEXT`, `XLRX_PDFTOPPM`, `XLRX_TESSERACT`; default:
    /// found in `PATH`) and the OCR languages (`XLRX_OCR_LANGS`, default `deu+eng`).
    pub pdftotext: PathBuf,
    pub pdftoppm: PathBuf,
    pub tesseract: PathBuf,
    pub ocr_langs: String,
    /// Parallel text extractions (`XLRX_EXTRACT_WORKERS`, default 1; 0 turns extraction off).
    pub extract_workers: u32,
    /// Data class of folders without a setting (`XLRX_DEFAULT_DATA_CLASS`: `local` or `cloud`,
    /// default `local`: nothing leaves the house until allowed per folder, PLAN 7.4).
    pub default_data_class: crate::files::data_class::Class,
    /// Largest file accepted through a public link, in bytes (`XLRX_LINK_UPLOAD_MAX_MB`, default
    /// 10 240 MB).
    pub link_upload_max: u64,
    /// Time zone of the household, for "opened on Mondays" (`XLRX_TIMEZONE`, default
    /// `Europe/Berlin`; checked against the database at start).
    pub timezone: String,
    /// The outside cache in S3 (PLAN 15.2); off without `XLRX_S3_BUCKET`.
    pub s3: Option<crate::s3::S3Config>,
    pub mirror: MirrorConfig,
    /// Further networks counted as the home network (`XLRX_LAN_NETS`, e.g. the home IPv6
    /// prefix): nothing is redirected to the outside cache for them. Private ranges always count.
    pub lan_nets: Vec<crate::files::mirror::Cidr>,
    /// AI search (PLAN 6.3, 7).
    pub ai: AiConfig,
}

/// AI analysis (PLAN 7): a provider in the cloud for "Cloud erlaubt", one in the home network for
/// "Nur lokal". Both speak the OpenAI API (`/v1/embeddings`, `/v1/chat/completions`).
#[derive(Clone, Debug)]
pub struct AiConfig {
    pub cloud: Option<CloudAi>,
    pub local: Option<LocalAi>,
    /// Spending limit per month in euros (`XLRX_AI_BUDGET_EUR`, default 20); the administration
    /// can change it.
    pub budget: f64,
    /// Requests to the cloud at the same time (`XLRX_AI_WORKERS`, default 2).
    pub cloud_workers: u32,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            cloud: None,
            local: None,
            budget: 20.0,
            cloud_workers: 2,
        }
    }
}

/// The provider for "Cloud erlaubt" (e.g. Scaleway, `XLRX_AI_URL`).
#[derive(Clone, Debug)]
pub struct CloudAi {
    /// Base address up to `/v1` (`https://api.scaleway.ai/v1`).
    pub url: Url,
    /// `XLRX_AI_KEY_FILE` (a Docker secret) or `XLRX_AI_KEY`.
    pub key: String,
    /// `XLRX_AI_EMBED_MODEL` (default `qwen3-embedding-8b`) with `XLRX_AI_EMBED_DIM` dimensions
    /// (default 1024). One model per installation: changing it embeds everything again.
    pub embed_model: String,
    pub embed_dim: u32,
    /// Picture analysis (`XLRX_AI_VISION_MODEL`, default `gemma-4-26b-a4b-it`; `off`: none).
    pub vision_model: Option<String>,
    pub prices: Prices,
}

/// Euros per million tokens (`XLRX_AI_PRICE_EMBED`, `XLRX_AI_PRICE_VISION_IN`,
/// `XLRX_AI_PRICE_VISION_OUT`), for the budget and the cost estimate.
#[derive(Clone, Copy, Debug)]
pub struct Prices {
    pub embed: f64,
    pub vision_in: f64,
    pub vision_out: f64,
}

/// The service in the home network for "Nur lokal" (`embed-local`, `XLRX_LOCAL_AI_URL`).
#[derive(Clone, Debug)]
pub struct LocalAi {
    pub url: Url,
    /// `XLRX_LOCAL_EMBED_MODEL` (default `multilingual-e5-small`, `XLRX_LOCAL_EMBED_DIM` 384).
    pub embed_model: String,
    pub embed_dim: u32,
    /// Picture vectors (`XLRX_LOCAL_CLIP_MODEL`, default `clip-ViT-B-32-multilingual-v1`, 512
    /// dimensions in `XLRX_LOCAL_CLIP_DIM`; `off`: none).
    pub clip_model: Option<String>,
    pub clip_dim: u32,
}

/// What the outside cache may hold and how fast it fills.
#[derive(Clone, Debug)]
pub struct MirrorConfig {
    /// Total size (`XLRX_S3_BUDGET_GB`, default 200).
    pub budget: u64,
    /// Objects not held by a link go after this many days (`XLRX_S3_MAX_DAYS`, default 30).
    pub max_days: u32,
    /// Smaller files are served from the NAS (`XLRX_S3_MIN_MB`, default 8).
    pub min_size: u64,
    /// Upload limit in bytes per second (`XLRX_S3_UPLOAD_MBIT`, default 20; 0: none).
    pub upload_rate: Option<u64>,
    /// Hours (local time) for prefetching, start and end (`XLRX_S3_PREFETCH_HOURS`, default 1-6).
    pub prefetch_hours: (u32, u32),
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            budget: 200_000_000_000,
            max_days: 30,
            min_size: 8_000_000,
            upload_rate: Some(20 * 125_000),
            prefetch_hours: (1, 6),
        }
    }
}

/// Parameters for argon2id. Calibrate on the DS918+ (J3455) so that one check takes ~250 ms
/// (`xlrx-server bench-argon2`).
#[derive(Clone, Copy, Debug)]
pub struct ArgonParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl Default for ArgonParams {
    fn default() -> Self {
        // OWASP recommendation for argon2id; fine-tuning in the M0 spike.
        Self {
            m_kib: 19 * 1024,
            t: 2,
            p: 1,
        }
    }
}

/// Fixed key used by the local trial setup (`deploy/docker-compose.local.yml`, `scripts/dev.sh`).
/// It is public, so the server refuses it for an `https` deployment.
pub const LOCAL_DEV_KEY: &[u8; 32] = b"xlrx-local-development-only-key!";

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// A model name as the provider knows it. Model names end up in index definitions, so only
/// harmless characters are accepted.
pub fn model_name(var_name: &str, v: &str) -> Result<String, String> {
    let v = v.trim();
    if v.is_empty()
        || v.len() > 100
        || !v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/@+-".contains(c))
    {
        return Err(format!("{var_name}: „{v}“ ist kein gültiger Modellname"));
    }
    Ok(v.to_owned())
}

/// A model, a default one, or none with `off`.
fn optional_model(name: &str, default: &str) -> Result<Option<String>, String> {
    match var(name).as_deref().map(str::trim) {
        Some("off" | "aus" | "0" | "false") => Ok(None),
        Some(v) => model_name(name, v).map(Some),
        None => Ok(Some(default.to_owned())),
    }
}

fn float(name: &str, default: f64) -> Result<f64, String> {
    var(name).map_or(Ok(default), |v| {
        v.trim()
            .replace(',', ".")
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .ok_or_else(|| format!("{name}: „{v}“ ist keine Zahl ≥ 0"))
    })
}

fn dimension(name: &str, default: u32) -> Result<u32, String> {
    var(name).map_or(Ok(default), |v| {
        v.trim()
            .parse::<u32>()
            .ok()
            .filter(|d| (2..=4096).contains(d))
            .ok_or_else(|| format!("{name}: Dimension zwischen 2 und 4096"))
    })
}

impl AiConfig {
    fn from_env() -> Result<Self, String> {
        let cloud = match var("XLRX_AI_URL") {
            None => None,
            Some(url) => {
                let url: Url = url
                    .trim()
                    .parse()
                    .map_err(|e| format!("XLRX_AI_URL: {e}"))?;
                crate::ai::provider::check_cloud_url(&url)?;
                Some(CloudAi {
                    url,
                    key: secret("XLRX_AI_KEY")?,
                    embed_model: model_name(
                        "XLRX_AI_EMBED_MODEL",
                        &var("XLRX_AI_EMBED_MODEL").unwrap_or_else(|| "qwen3-embedding-8b".into()),
                    )?,
                    embed_dim: dimension("XLRX_AI_EMBED_DIM", 1024)?,
                    vision_model: optional_model("XLRX_AI_VISION_MODEL", "gemma-4-26b-a4b-it")?,
                    prices: Prices {
                        embed: float("XLRX_AI_PRICE_EMBED", 0.10)?,
                        vision_in: float("XLRX_AI_PRICE_VISION_IN", 0.25)?,
                        vision_out: float("XLRX_AI_PRICE_VISION_OUT", 0.50)?,
                    },
                })
            }
        };
        let local = match var("XLRX_LOCAL_AI_URL") {
            None => None,
            Some(url) => {
                let url: Url = url
                    .trim()
                    .parse()
                    .map_err(|e| format!("XLRX_LOCAL_AI_URL: {e}"))?;
                crate::ai::provider::check_home_url("XLRX_LOCAL_AI_URL", &url)?;
                Some(LocalAi {
                    url,
                    embed_model: model_name(
                        "XLRX_LOCAL_EMBED_MODEL",
                        &var("XLRX_LOCAL_EMBED_MODEL")
                            .unwrap_or_else(|| "multilingual-e5-small".into()),
                    )?,
                    embed_dim: dimension("XLRX_LOCAL_EMBED_DIM", 384)?,
                    clip_model: optional_model(
                        "XLRX_LOCAL_CLIP_MODEL",
                        "clip-ViT-B-32-multilingual-v1",
                    )?,
                    clip_dim: dimension("XLRX_LOCAL_CLIP_DIM", 512)?,
                })
            }
        };
        let workers = var("XLRX_AI_WORKERS")
            .map(|v| {
                v.trim()
                    .parse::<u32>()
                    .map_err(|e| format!("XLRX_AI_WORKERS: {e}"))
            })
            .transpose()?
            .unwrap_or(2)
            .clamp(1, 16);
        Ok(Self {
            cloud,
            local,
            budget: float("XLRX_AI_BUDGET_EUR", 20.0)?,
            cloud_workers: workers,
        })
    }
}

/// A secret from `NAME_FILE` (a Docker secret) or else `NAME`.
fn secret(name: &str) -> Result<String, String> {
    match (var(&format!("{name}_FILE")), var(name)) {
        (Some(path), _) => Ok(std::fs::read_to_string(&path)
            .map_err(|e| format!("{name}_FILE {path}: {e}"))?
            .trim()
            .to_owned()),
        (None, Some(v)) => Ok(v),
        (None, None) => Err(format!("{name}_FILE bzw. {name} fehlt")),
    }
}

/// Reads a 32-byte key (Base64 or hex).
pub fn parse_key(raw: &str) -> Result<[u8; 32], String> {
    let raw = raw.trim();
    let bytes = if raw.len() == 64 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        (0..32)
            .map(|i| u8::from_str_radix(&raw[2 * i..2 * i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
            .map_err(|e| e.to_string())?
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(raw)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw))
            .map_err(|_| "Schlüssel ist weder Base64 noch Hex".to_string())?
    };
    bytes
        .try_into()
        .map_err(|_| "Schlüssel muss genau 32 Bytes lang sein".to_string())
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        // The URL contains the DB password: preferably from a Docker secret.
        let database_url = match (var("DATABASE_URL_FILE"), var("DATABASE_URL")) {
            (Some(path), _) => std::fs::read_to_string(&path)
                .map_err(|e| format!("DATABASE_URL_FILE {path}: {e}"))?
                .trim()
                .to_owned(),
            (None, Some(v)) => v,
            (None, None) => return Err("DATABASE_URL_FILE bzw. DATABASE_URL fehlt".into()),
        };
        let bind = var("XLRX_BIND")
            .unwrap_or_else(|| "0.0.0.0:8080".into())
            .parse()
            .map_err(|e| format!("XLRX_BIND: {e}"))?;
        let public_url: Url = var("XLRX_PUBLIC_URL")
            .ok_or("XLRX_PUBLIC_URL fehlt (z.B. https://drive.example.de)")?
            .parse()
            .map_err(|e| format!("XLRX_PUBLIC_URL: {e}"))?;
        let extra_origins = var("XLRX_EXTRA_ORIGINS")
            .map(|v| {
                v.split(',')
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| {
                        s.trim()
                            .parse::<Url>()
                            .map_err(|e| format!("XLRX_EXTRA_ORIGINS: {e}"))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let rp_id = match var("XLRX_RP_ID") {
            Some(v) => v,
            None => public_url
                .host_str()
                .ok_or("XLRX_PUBLIC_URL ohne Host")?
                .to_owned(),
        };
        let key_raw = match (var("XLRX_SECRET_KEY_FILE"), var("XLRX_SECRET_KEY")) {
            (Some(path), _) => std::fs::read_to_string(&path)
                .map_err(|e| format!("XLRX_SECRET_KEY_FILE {path}: {e}"))?,
            (None, Some(v)) => v,
            (None, None) => {
                return Err(
                    "XLRX_SECRET_KEY_FILE fehlt (Schlüssel erzeugen: xlrx-server gen-secret)"
                        .into(),
                );
            }
        };
        let secret_key = parse_key(&key_raw)?;
        if public_url.scheme() == "https" && &secret_key == LOCAL_DEV_KEY {
            return Err(
                "Der lokale Entwicklungsschlüssel darf nicht produktiv verwendet werden \
                 (xlrx-server gen-secret)"
                    .into(),
            );
        }
        let num = |name: &str, default: u32| -> Result<u32, String> {
            var(name).map_or(Ok(default), |v| {
                v.parse().map_err(|e| format!("{name}: {e}"))
            })
        };
        let d = ArgonParams::default();
        Ok(Self {
            database_url,
            bind,
            public_url,
            extra_origins,
            rp_id,
            secret_key,
            web_dir: var("XLRX_WEB_DIR").map(PathBuf::from),
            trust_proxy: var("XLRX_TRUST_PROXY").is_some_and(|v| v == "1" || v == "true"),
            argon2: ArgonParams {
                m_kib: num("XLRX_ARGON2_M_KIB", d.m_kib)?,
                t: num("XLRX_ARGON2_T", d.t)?,
                p: num("XLRX_ARGON2_P", d.p)?,
            },
            password_blocklist: var("XLRX_PASSWORD_BLOCKLIST").map(PathBuf::from),
            data_dir: var("XLRX_DATA_DIR").map(PathBuf::from),
            state_dir: var("XLRX_STATE_DIR")
                .map(PathBuf::from)
                .or_else(|| var("XLRX_DATA_DIR").map(|d| PathBuf::from(d).join("xlrx-state"))),
            home_pattern: var("XLRX_HOME_PATTERN").unwrap_or_else(|| "homes/{user}/Drive".into()),
            force_copy: false,
            watch: var("XLRX_WATCH").is_none_or(|v| v != "0" && v != "false"),
            device_confirm_days: num("XLRX_DEVICE_CONFIRM_DAYS", 30)?.max(1),
            tika_url: var("XLRX_TIKA_URL")
                .filter(|v| !v.trim().is_empty())
                .map(|v| {
                    let url: Url = v
                        .trim()
                        .parse()
                        .map_err(|e| format!("XLRX_TIKA_URL: {e}"))?;
                    crate::extract::tika::check_url(&url)?;
                    Ok::<_, String>(url)
                })
                .transpose()?,
            pdftotext: var("XLRX_PDFTOTEXT")
                .unwrap_or_else(|| "pdftotext".into())
                .into(),
            pdftoppm: var("XLRX_PDFTOPPM")
                .unwrap_or_else(|| "pdftoppm".into())
                .into(),
            tesseract: var("XLRX_TESSERACT")
                .unwrap_or_else(|| "tesseract".into())
                .into(),
            ocr_langs: var("XLRX_OCR_LANGS").unwrap_or_else(|| "deu+eng".into()),
            extract_workers: num("XLRX_EXTRACT_WORKERS", 1)?,
            default_data_class: match var("XLRX_DEFAULT_DATA_CLASS").as_deref() {
                None | Some("") => crate::files::data_class::Class::Local,
                Some(v) => crate::files::data_class::Class::parse(v.trim()).ok_or(
                    "XLRX_DEFAULT_DATA_CLASS: „local“ (Nur lokal) oder „cloud“ (Cloud erlaubt)",
                )?,
            },
            link_upload_max: u64::from(num("XLRX_LINK_UPLOAD_MAX_MB", 10_240)?) * 1_000_000,
            s3: match var("XLRX_S3_BUCKET").filter(|b| !b.trim().is_empty()) {
                None => None,
                Some(bucket) => Some(crate::s3::S3Config {
                    endpoint: var("XLRX_S3_ENDPOINT")
                        .ok_or(
                            "XLRX_S3_ENDPOINT fehlt (z. B. https://fsn1.your-objectstorage.com)",
                        )?
                        .trim()
                        .parse()
                        .map_err(|e| format!("XLRX_S3_ENDPOINT: {e}"))?,
                    bucket: bucket.trim().to_owned(),
                    region: var("XLRX_S3_REGION").unwrap_or_else(|| "us-east-1".into()),
                    access_key: secret("XLRX_S3_ACCESS_KEY")?,
                    secret_key: secret("XLRX_S3_SECRET_KEY")?,
                    virtual_host: var("XLRX_S3_VIRTUAL_HOST")
                        .is_some_and(|v| v == "1" || v == "true"),
                }),
            },
            mirror: MirrorConfig {
                budget: u64::from(num("XLRX_S3_BUDGET_GB", 200)?) * 1_000_000_000,
                max_days: num("XLRX_S3_MAX_DAYS", 30)?.max(1),
                min_size: u64::from(num("XLRX_S3_MIN_MB", 8)?) * 1_000_000,
                upload_rate: match num("XLRX_S3_UPLOAD_MBIT", 20)? {
                    0 => None,
                    m => Some(u64::from(m) * 125_000),
                },
                prefetch_hours: match var("XLRX_S3_PREFETCH_HOURS") {
                    None => (1, 6),
                    Some(v) => v
                        .split_once('-')
                        .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)))
                        .filter(|(a, b): &(u32, u32)| *a < 24 && *b <= 24)
                        .ok_or("XLRX_S3_PREFETCH_HOURS: z. B. 1-6")?,
                },
            },
            lan_nets: var("XLRX_LAN_NETS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(crate::files::mirror::Cidr::parse)
                .collect::<Result<_, _>>()
                .map_err(|e| format!("XLRX_LAN_NETS: {e}"))?,
            ai: AiConfig::from_env()?,
            timezone: var("XLRX_TIMEZONE")
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "Europe/Berlin".into()),
        })
    }

    /// Allowed origins (scheme + host + port) for state-changing requests.
    pub fn allowed_origins(&self) -> Vec<String> {
        std::iter::once(&self.public_url)
            .chain(self.extra_origins.iter())
            .map(|u| u.origin().ascii_serialization())
            .collect()
    }

    pub fn secure_cookies(&self) -> bool {
        self.public_url.scheme() == "https"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_in_hex_and_base64() {
        let hex = "00".repeat(31) + "ff";
        assert_eq!(parse_key(&hex).unwrap()[31], 0xff);
        let b64 = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        assert_eq!(parse_key(&format!("{b64}\n")).unwrap(), [7u8; 32]);
        assert!(parse_key("zu kurz").is_err());
        assert!(parse_key(&base64::engine::general_purpose::STANDARD.encode([1u8; 16])).is_err());
    }
}
