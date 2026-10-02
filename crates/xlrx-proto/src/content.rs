use std::fmt;

use serde::{Deserialize, Serialize};

/// Inhalts-Hash einer Datei (Schema `xlrx-content-v1`, siehe `xlrx-chunk`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    /// Hex-Darstellung (64 Zeichen), z.B. für Objektnamen im Speicher.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
            s.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
        }
        s
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Kurzform reicht für Logs und Tests.
        write!(f, "h{}", &self.to_hex()[..12])
    }
}

/// Inhalt einer Datei: Hash und Größe. Zwei Dateien mit gleichem `FileContent` gelten als inhaltsgleich.
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
