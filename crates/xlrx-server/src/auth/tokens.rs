//! Random tokens (sessions, invites, intermediate steps) and recovery codes.
//!
//! The database only ever stores the SHA-256 of a token. Tokens carry 256 bits of randomness,
//! recovery codes 80 bits; a fast hash is therefore sufficient.

use base64::Engine as _;
use rand::Rng as _;
use sha2::{Digest, Sha256};

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::rng().fill_bytes(&mut b);
    b
}

/// New token (Base64url, 43 characters) and its hash.
pub fn new_token() -> (String, Vec<u8>) {
    let t = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<32>());
    let h = hash_token(&t);
    (t, h)
}

pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// A recovery code "ABCD-EFGH-JKLM-NPQR" (16 Base32 characters = 80 bits).
pub fn new_recovery_code() -> String {
    let bytes: [u8; 10] = random_bytes();
    let mut bits: u128 = 0;
    for b in bytes {
        bits = (bits << 8) | u128::from(b);
    }
    let chars: Vec<char> = (0..16)
        .rev()
        .map(|i| B32[((bits >> (i * 5)) & 31) as usize] as char)
        .collect();
    chars
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// Normalizes an input (upper/lower case, hyphens, whitespace).
pub fn normalize_recovery_code(input: &str) -> Option<String> {
    let s: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (s.len() == 16 && s.bytes().all(|b| B32.contains(&b))).then_some(s)
}

pub fn hash_recovery_code(normalized: &str) -> Vec<u8> {
    Sha256::digest(format!("xlrx recovery v1 {normalized}").as_bytes()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_hashed() {
        let (a, ha) = new_token();
        let (b, _) = new_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert_eq!(hash_token(&a), ha);
    }

    #[test]
    fn recovery_codes_roundtrip() {
        let c = new_recovery_code();
        assert_eq!(c.len(), 19);
        let n = normalize_recovery_code(&c.to_lowercase().replace('-', " ")).unwrap();
        assert_eq!(n, c.replace('-', ""));
        assert!(normalize_recovery_code("ABCD-EFGH").is_none());
        assert!(
            normalize_recovery_code("ABCD-EFGH-IJKL-MN01").is_none(),
            "0 und 1 sind kein Base32"
        );
    }
}
