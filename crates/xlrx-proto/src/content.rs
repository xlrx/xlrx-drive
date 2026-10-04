use std::fmt;

use serde::{Deserialize, Serialize};

/// Content hash of a file (schema `xlrx-content-v1`, see `xlrx-chunk`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    /// Hex representation (64 characters), e.g. for object names in storage.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
            s.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
        }
        s
    }

    /// Parses the hex representation: exactly 64 hex digits, upper or lower case.
    pub fn from_hex(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() != 64 {
            return None;
        }
        let digit = |c: u8| {
            char::from(c)
                .to_digit(16)
                .and_then(|d| u8::try_from(d).ok())
        };
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = (digit(b[2 * i])? << 4) | digit(b[2 * i + 1])?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The short form is enough for logs and tests.
        write!(f, "h{}", &self.to_hex()[..12])
    }
}

/// File content: hash and size. Two files with equal `FileContent` are considered identical.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileContent {
    pub hash: ContentHash,
    pub size: u64,
}

impl fmt::Debug for FileContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}/{}B", self.hash, self.size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_hin_und_zurueck() {
        let mut h = [0u8; 32];
        for (i, b) in h.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37) ^ 0xa5;
        }
        let hash = ContentHash(h);
        let hex = hash.to_hex();
        assert_eq!(ContentHash::from_hex(&hex), Some(hash));
        assert_eq!(ContentHash::from_hex(&hex.to_uppercase()), Some(hash));
        assert_eq!(ContentHash::from_hex(&hex[1..]), None);
        assert_eq!(ContentHash::from_hex(&format!("{hex}0")), None);
        assert_eq!(ContentHash::from_hex(&format!("+{}", &hex[1..])), None);
        assert_eq!(ContentHash::from_hex(&format!("g{}", &hex[1..])), None);
        // Multi-byte characters never count as hex digits (and never split a char).
        assert_eq!(ContentHash::from_hex(&format!("ä{}", &hex[2..])), None);
    }
}
