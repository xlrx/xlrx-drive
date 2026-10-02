//! Konfiguration aus Umgebungsvariablen (siehe `deploy/docker-compose.yml`).

use std::net::SocketAddr;
use std::path::PathBuf;

use base64::Engine as _;
use url::Url;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub bind: SocketAddr,
    /// Öffentliche Adresse, z.B. `https://drive.example.de`. Bestimmt Cookie-`Secure`, erlaubte
    /// Origin und den Passkey-Ursprung.
    pub public_url: Url,
    /// Weitere erlaubte Origins, z.B. der LAN-Endpunkt `https://drive-lan.example.de` (PLAN 5.9).
    pub extra_origins: Vec<Url>,
    /// Relying-Party-ID für Passkeys (Domain). Standard: Host der öffentlichen Adresse. Einmal festlegen:
    /// Eine spätere Änderung macht alle registrierten Passkeys unbrauchbar.
    pub rp_id: String,
    /// Schlüssel für Geheimnisse in der DB (TOTP). Kommt aus einem Docker-Secret, nie aus der DB.
    pub secret_key: [u8; 32],
    /// Gebaute Web-App (SvelteKit, statisch). Ohne: nur API.
    pub web_dir: Option<PathBuf>,
    /// `X-Forwarded-For` des vorgelagerten Proxys (Caddy) auswerten.
    pub trust_proxy: bool,
    pub argon2: ArgonParams,
    /// Zusätzliche Liste verbotener Passwörter (eine Zeile je Passwort).
    pub password_blocklist: Option<PathBuf>,
}

/// Parameter für argon2id. Auf dem DS918+ (J3455) so kalibrieren, dass eine Prüfung ~250 ms dauert
/// (`xlrx-server bench-argon2`).
#[derive(Clone, Copy, Debug)]
pub struct ArgonParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl Default for ArgonParams {
    fn default() -> Self {
        // OWASP-Empfehlung für argon2id; Feinabstimmung im M0-Spike.
        Self {
            m_kib: 19 * 1024,
            t: 2,
            p: 1,
        }
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Liest einen 32-Byte-Schlüssel (Base64 oder Hex).
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
        // Die URL enthält das DB-Passwort: bevorzugt aus einem Docker-Secret.
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
        })
    }

    /// Erlaubte Origins (Schema + Host + Port) für zustandsändernde Anfragen.
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
