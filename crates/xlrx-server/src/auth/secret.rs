//! Verschlüsselung von Geheimnissen in der Datenbank (TOTP) mit AES-256-GCM.
//!
//! Format: 12 Byte Nonce ‖ Chiffrat mit Tag. Die zusätzlichen Daten (AAD) binden ein Geheimnis an
//! seinen Zweck und sein Konto: Ein in der DB vertauschtes Geheimnis lässt sich nicht entschlüsseln.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};

use super::tokens::random_bytes;

pub struct SecretBox {
    cipher: Aes256Gcm,
}

impl SecretBox {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new_from_slice(key).expect("32-Byte-Schlüssel"),
        }
    }

    pub fn seal(&self, aad: &[u8], plain: &[u8]) -> Vec<u8> {
        let nonce: [u8; 12] = random_bytes();
        let ct = self
            .cipher
            .encrypt(&Nonce::from(nonce), Payload { msg: plain, aad })
            .expect("AES-GCM-Verschlüsselung");
        let mut out = Vec::with_capacity(12 + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    pub fn open(&self, aad: &[u8], data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < 12 + 16 {
            return None;
        }
        let (nonce, ct) = data.split_at(12);
        let nonce: [u8; 12] = nonce.try_into().ok()?;
        self.cipher
            .decrypt(&Nonce::from(nonce), Payload { msg: ct, aad })
            .ok()
    }
}

/// AAD für das TOTP-Geheimnis eines Kontos.
pub fn totp_aad(user_id: i64) -> Vec<u8> {
    format!("xlrx totp v1 user {user_id}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_binding() {
        let b = SecretBox::new(&[9u8; 32]);
        let sealed = b.seal(b"a", b"geheim");
        assert_eq!(b.open(b"a", &sealed).as_deref(), Some(&b"geheim"[..]));
        assert_eq!(b.open(b"b", &sealed), None, "anderes Konto");
        let other = SecretBox::new(&[8u8; 32]);
        assert_eq!(other.open(b"a", &sealed), None, "anderer Schlüssel");
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(b.open(b"a", &tampered), None);
        assert_ne!(b.seal(b"a", b"x"), b.seal(b"a", b"x"), "frische Nonce");
    }
}
