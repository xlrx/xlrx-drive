use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use unicode_normalization::UnicodeNormalization;

/// Maximum length of a name in bytes (UTF-8). Matches the limit of APFS, Btrfs and ext4.
pub const MAX_NAME_BYTES: usize = 255;

/// A single file or directory name, always in Unicode NFC.
///
/// macOS often returns names in NFD ("Ä" as A + diaeresis). Internally only NFC is used, so
/// that the same name compares identically on all platforms.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("Name ist leer")]
    Empty,
    #[error("„.“ und „..“ sind als Namen nicht erlaubt")]
    Reserved,
    #[error("Name enthält ein unzulässiges Zeichen ('/' oder NUL)")]
    InvalidChar,
    #[error("Name ist länger als {MAX_NAME_BYTES} Bytes")]
    TooLong,
}

impl Name {
    /// Validates and normalizes (NFC) a name.
    pub fn new(raw: &str) -> Result<Self, NameError> {
        let nfc: String = raw.nfc().collect();
        if nfc.is_empty() {
            return Err(NameError::Empty);
        }
        if nfc == "." || nfc == ".." {
            return Err(NameError::Reserved);
        }
        if nfc.contains(['/', '\0']) {
            return Err(NameError::InvalidChar);
        }
        if nfc.len() > MAX_NAME_BYTES {
            return Err(NameError::TooLong);
        }
        Ok(Self(nfc))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Comparison key for case-insensitive file systems (APFS default, SMB).
    ///
    /// Unicode lowercasing plus NFC. This covers the cases that matter in practice.
    /// Exotic differences between the case-folding tables of different file systems
    /// (e.g. "ß"/"ss") are not merged; that is the more cautious choice.
    pub fn fold_key(&self) -> String {
        self.0.to_lowercase().nfc().collect()
    }

    /// Splits into stem and extension ("Bericht.final.pdf" → ("Bericht.final", ".pdf")).
    /// Hidden files without a further extension (".bashrc") have no extension.
    pub fn split_extension(&self) -> (&str, &str) {
        match self.0.rfind('.') {
            Some(0) | None => (&self.0, ""),
            Some(i) => self.0.split_at(i),
        }
    }

    /// Builds a derived name "stem (suffix).ext" and shortens the stem if needed so that the
    /// length limit is respected.
    ///
    /// The result is always valid, and different suffixes yield different names (the loops that
    /// search for a free conflict name rely on this). If necessary, the extension is counted as
    /// part of the stem and the suffix is truncated at the front; its end (the counter) is kept.
    pub fn with_suffix(&self, suffix: &str) -> Name {
        let clean: String = suffix
            .chars()
            .map(|c| if c == '/' || c == '\0' { '-' } else { c })
            .collect();
        let mut addition = format!(" ({clean})");
        if addition.len() >= MAX_NAME_BYTES {
            let mut start = addition.len() - (MAX_NAME_BYTES - 1);
            while !addition.is_char_boundary(start) {
                start += 1;
            }
            addition = addition[start..].to_owned();
        }
        let (mut stem, mut ext) = self.split_extension();
        if addition.len() + ext.len() >= MAX_NAME_BYTES {
            // Overlong "extension" (e.g. "1. Chapter …"): treat it as part of the stem.
            stem = self.as_str();
            ext = "";
        }
        let budget = MAX_NAME_BYTES - addition.len() - ext.len();
        let mut cut = stem.len().min(budget);
        loop {
            while !stem.is_char_boundary(cut) {
                cut -= 1;
            }
            // NFC composition can change the length: when in doubt, keep shortening.
            if let Ok(n) = Name::new(&format!("{}{addition}{ext}", &stem[..cut])) {
                return n;
            }
            if cut == 0 {
                break;
            }
            cut -= 1;
        }
        // Just the suffix: valid because it starts with ` (` (or a part of it) and fits.
        Name::new(&addition).unwrap_or_else(|_| Name(format!("Datei {}", addition.len())))
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Name {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Name {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Name::new(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn nfd_and_nfc_are_equal() {
        let nfd = "A\u{0308}pfel.txt"; // Ä as A + combining diaeresis
        let nfc = "\u{00C4}pfel.txt";
        assert_eq!(Name::new(nfd).unwrap(), Name::new(nfc).unwrap());
    }

    #[test]
    fn rejects_invalid_names() {
        assert_eq!(Name::new(""), Err(NameError::Empty));
        assert_eq!(Name::new("."), Err(NameError::Reserved));
        assert_eq!(Name::new(".."), Err(NameError::Reserved));
        assert_eq!(Name::new("a/b"), Err(NameError::InvalidChar));
        assert_eq!(Name::new("a\0b"), Err(NameError::InvalidChar));
        assert_eq!(Name::new(&"x".repeat(256)), Err(NameError::TooLong));
    }

    #[test]
    fn fold_key_ignores_case() {
        assert_eq!(
            Name::new("Bericht.PDF").unwrap().fold_key(),
            Name::new("bericht.pdf").unwrap().fold_key()
        );
    }

    #[test]
    fn suffix_keeps_extension() {
        let n = Name::new("Bericht.final.pdf").unwrap();
        assert_eq!(
            n.with_suffix("Konflikt").as_str(),
            "Bericht.final (Konflikt).pdf"
        );
        let hidden = Name::new(".bashrc").unwrap();
        assert_eq!(hidden.with_suffix("2").as_str(), ".bashrc (2)");
    }

    #[test]
    fn serde_roundtrip_normalizes() {
        let json = "\"A\\u0308pfel\"";
        let n: Name = serde_json::from_str(json).unwrap();
        assert_eq!(n.as_str(), "\u{00C4}pfel");
    }

    #[test]
    fn suffix_on_overlong_extension_stays_valid_and_distinct() {
        // Everything from the first dot would be the "extension", leaving no room for the suffix.
        let n = Name::new(&format!("1. {}", "x".repeat(240))).unwrap();
        let a = n.with_suffix("Konflikt Gerät0 1");
        let b = n.with_suffix("Konflikt Gerät0 2");
        assert!(Name::new(a.as_str()).is_ok());
        assert!(a.as_str().len() <= MAX_NAME_BYTES);
        assert_ne!(a, b);
    }

    #[test]
    fn overlong_suffix_keeps_its_end() {
        let n = Name::new("a.txt").unwrap();
        let long = "ü".repeat(200);
        let a = n.with_suffix(&format!("{long} 1"));
        let b = n.with_suffix(&format!("{long} 2"));
        assert!(a.as_str().len() <= MAX_NAME_BYTES);
        assert_ne!(a, b);
    }

    proptest! {
        #[test]
        fn suffix_always_valid_and_distinct(
            stem in "[^/\\x00]{1,300}",
            suffix in "[a-zA-Z0-9 äöü~]{1,40}",
            k in 1u32..1000,
        ) {
            if let Ok(n) = Name::new(&stem) {
                let a = n.with_suffix(&format!("{suffix} {k}"));
                let b = n.with_suffix(&format!("{suffix} {}", k + 1));
                prop_assert!(a.as_str().len() <= MAX_NAME_BYTES);
                prop_assert!(Name::new(a.as_str()).is_ok());
                prop_assert_eq!(Name::new(a.as_str()).unwrap(), a.clone());
                prop_assert_ne!(a, b);
            }
        }
    }
}
