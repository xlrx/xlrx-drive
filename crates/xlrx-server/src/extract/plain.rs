//! Text files read directly: UTF-8, UTF-16 (with byte order mark) or Latin-1.

/// Bytes looked at for a byte order mark and binary content.
const SNIFF: usize = 8192;

/// The text of a file's bytes, `None` if they look binary.
pub fn decode(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        return Some(String::from_utf8_lossy(rest).into_owned());
    }
    if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
        return Some(utf16(rest, u16::from_le_bytes));
    }
    if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
        return Some(utf16(rest, u16::from_be_bytes));
    }
    if bytes[..bytes.len().min(SNIFF)].contains(&0) {
        return None;
    }
    Some(match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        // A cut in the middle of a character at the read limit is no reason for Latin-1.
        Err(e) if e.error_len().is_none() => String::from_utf8_lossy(bytes).into_owned(),
        // Older Windows and Mac files: Latin-1 covers German umlauts.
        Err(_) => bytes.iter().map(|&b| char::from(b)).collect(),
    })
}

fn utf16(bytes: &[u8], word: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| word([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings() {
        assert_eq!(decode("Grüße".as_bytes()).unwrap(), "Grüße");
        assert_eq!(decode(b"\xEF\xBB\xBFGr\xC3\xBC\xC3\x9Fe").unwrap(), "Grüße");
        assert_eq!(decode(b"Gr\xFC\xDFe").unwrap(), "Grüße");
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("Grüße".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode(&le).unwrap(), "Grüße");
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain("Grüße".encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        assert_eq!(decode(&be).unwrap(), "Grüße");
        // Cut inside "ü": still UTF-8.
        assert_eq!(decode(b"Gr\xC3").unwrap(), "Gr\u{FFFD}");
        assert_eq!(decode(b"\x7FELF\x02\x01\x00\x00"), None);
    }
}
