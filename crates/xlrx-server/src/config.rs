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
