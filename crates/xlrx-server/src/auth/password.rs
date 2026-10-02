//! Passwörter: argon2id, Mindestanforderungen und lokale Sperrliste (ohne Anfrage nach außen).

use std::collections::HashSet;
use std::path::Path;

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};

use crate::config::ArgonParams;

pub const MIN_LEN: usize = 12;
pub const MAX_LEN: usize = 1024;

pub struct Passwords {
    argon: Argon2<'static>,
    /// Hash eines zufälligen Passworts: Bei unbekannten Konten wird dagegen geprüft, damit
    /// Antwort und Antwortzeit gleich sind wie bei einem falschen Passwort.
    dummy: String,
    blocklist: HashSet<String>,
}

impl Passwords {
    pub fn new(p: ArgonParams, extra_blocklist: Option<&Path>) -> Result<Self, String> {
        let params = Params::new(p.m_kib, p.t, p.p, None).map_err(|e| e.to_string())?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut blocklist: HashSet<String> = include_str!("common-passwords.txt")
            .lines()
            .map(|l| l.trim().to_lowercase())
            .filter(|l| !l.is_empty())
            .collect();
        if let Some(path) = extra_blocklist {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("Sperrliste {}: {e}", path.display()))?;
            blocklist.extend(
                text.lines()
                    .map(|l| l.trim().to_lowercase())
                    .filter(|l| l.chars().count() >= MIN_LEN),
            );
        }
        let mut this = Self {
            argon,
            dummy: String::new(),
            blocklist,
        };
        let random = crate::auth::tokens::new_token().0;
        this.dummy = this.hash(&random)?;
        Ok(this)
    }

    pub fn hash(&self, password: &str) -> Result<String, String> {
        let salt: [u8; 16] = crate::auth::tokens::random_bytes();
        self.argon
            .hash_password_with_salt(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| e.to_string())
    }

    /// Prüft ein Passwort. Ohne gespeicherten Hash wird gegen den Platzhalter geprüft (gleiche Dauer)
    /// und immer `false` geliefert.
    pub fn verify(&self, stored: Option<&str>, password: &str) -> bool {
        let (hash, real) = match stored {
            Some(h) => (h, true),
            None => (self.dummy.as_str(), false),
        };
        // Die Parameter stehen im Hash: alte Hashes bleiben nach einer Neukalibrierung gültig.
        let ok = self
            .argon
            .verify_password(password.as_bytes(), hash)
            .is_ok();
        ok && real
    }

    /// Mindestanforderungen. Fehlertext für die Oberfläche.
    pub fn check_policy(&self, password: &str, username: &str) -> Result<(), String> {
        let n = password.chars().count();
        if n < MIN_LEN {
            return Err(format!(
                "Das Passwort muss mindestens {MIN_LEN} Zeichen lang sein."
            ));
        }
        if n > MAX_LEN {
            return Err("Das Passwort ist zu lang.".into());
        }
        let lower = password.to_lowercase();
        if self.blocklist.contains(&lower) {
            return Err("Dieses Passwort ist zu verbreitet. Bitte ein anderes wählen.".into());
        }
        let user = username.to_lowercase();
        if user.chars().count() >= 3 && lower.contains(&user) {
            return Err("Das Passwort darf den Benutzernamen nicht enthalten.".into());
        }
        let distinct: HashSet<char> = lower.chars().collect();
        if distinct.len() < 5 {
            return Err("Das Passwort besteht aus zu wenigen verschiedenen Zeichen.".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast() -> Passwords {
        Passwords::new(
            ArgonParams {
                m_kib: 256,
                t: 1,
                p: 1,
            },
            None,
        )
        .unwrap()
    }

    #[test]
    fn hash_and_verify() {
        let p = fast();
        let h = p.hash("korrekt Pferd Batterie").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(p.verify(Some(&h), "korrekt Pferd Batterie"));
        assert!(!p.verify(Some(&h), "korrekt Pferd Batteri"));
        assert!(!p.verify(None, "korrekt Pferd Batterie"));
        assert!(!p.verify(Some("kein hash"), "x"));
    }

    #[test]
    fn policy() {
        let p = fast();
        assert!(p.check_policy("kurz", "anna").is_err());
        assert!(
            p.check_policy("Passwort1234", "anna").is_err(),
            "Sperrliste, ohne Groß/klein"
        );
        assert!(
            p.check_policy("anna-ist-toll-2026", "anna").is_err(),
            "enthält Benutzernamen"
        );
        assert!(
            p.check_policy("abababababab", "anna").is_err(),
            "zu wenige Zeichen"
        );
        assert!(p.check_policy("Wolken über dem Garten", "anna").is_ok());
    }
}
