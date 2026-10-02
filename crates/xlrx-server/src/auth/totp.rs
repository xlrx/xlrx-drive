//! TOTP per RFC 6238 (HMAC-SHA1, 6 digits, 30 s, ±1 time step), compatible with any authenticator
//! app. Deliberately implemented in-house (a few lines, verified against the RFC test vectors).

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use subtle::ConstantTimeEq;

pub const STEP_SECS: u64 = 30;
pub const DIGITS: usize = 6;
pub const ISSUER: &str = "xlrx-drive";

/// New secret (160 bits, as recommended by RFC 4226).
pub fn new_secret() -> Vec<u8> {
    crate::auth::tokens::random_bytes::<20>().to_vec()
}

/// HOTP (RFC 4226) for one time step.
pub fn code_at_step(secret: &[u8], step: u64) -> String {
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(secret)
        .expect("HMAC akzeptiert jede Schlüssellänge");
    mac.update(&step.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let offset = usize::from(h[h.len() - 1] & 0x0f);
    let bin = (u32::from(h[offset] & 0x7f) << 24)
        | (u32::from(h[offset + 1]) << 16)
        | (u32::from(h[offset + 2]) << 8)
        | u32::from(h[offset + 3]);
    format!("{:06}", bin % 1_000_000)
}

pub fn current_step(now_unix: u64) -> u64 {
    now_unix / STEP_SECS
}

/// Checks a code against the time steps t-1, t, t+1 and returns the matching step.
/// Steps up to and including `last_used` are rejected (each code is valid only once).
pub fn verify(secret: &[u8], code: &str, now_unix: u64, last_used: Option<i64>) -> Option<i64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let t = current_step(now_unix);
    let mut found = None;
    // Check all candidates (same running time regardless of which one matches).
    for step in [t.saturating_sub(1), t, t + 1] {
        let expected = code_at_step(secret, step);
        let hit: bool = expected.as_bytes().ct_eq(code.as_bytes()).into();
        let step = i64::try_from(step).ok()?;
        if hit && last_used.is_none_or(|l| step > l) && found.is_none() {
            found = Some(step);
        }
    }
    found
}

/// Base32 (RFC 4648) without padding, as authenticator apps expect it.
pub fn secret_base32(secret: &[u8]) -> String {
    const A: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in secret {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            out.push(A[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(A[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn otpauth_url(secret: &[u8], account: &str) -> String {
    let enc = |s: &str| {
        url::form_urlencoded::byte_serialize(s.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    };
    format!(
        "otpauth://totp/{issuer}:{account}?secret={secret}&issuer={issuer}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        issuer = enc(ISSUER),
        account = enc(account),
        secret = secret_base32(secret),
    )
}

/// QR code of the otpauth URL as SVG.
pub fn qr_svg(url: &str) -> String {
    match qrcode::QrCode::new(url.as_bytes()) {
        Ok(code) => code
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(220, 220)
            .quiet_zone(true)
            .build(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238, Appendix B (SHA-1, secret "12345678901234567890"), last 6 digits.
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn rfc6238_vectors() {
        for (t, code) in [
            (59u64, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(code_at_step(RFC_SECRET, t / STEP_SECS), code, "T={t}");
        }
    }

    #[test]
    fn base32_rfc4648() {
        assert_eq!(secret_base32(b""), "");
        assert_eq!(secret_base32(b"f"), "MY");
        assert_eq!(secret_base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(
            secret_base32(RFC_SECRET),
            "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
        );
    }

    #[test]
    fn window_and_replay() {
        let now = 1_234_567_890;
        let t = current_step(now);
        let code = code_at_step(RFC_SECRET, t);
        assert_eq!(verify(RFC_SECRET, &code, now, None), Some(t as i64));
        assert_eq!(
            verify(RFC_SECRET, &code, now, Some(t as i64)),
            None,
            "Replay"
        );
        let prev = code_at_step(RFC_SECRET, t - 1);
        assert_eq!(verify(RFC_SECRET, &prev, now, None), Some(t as i64 - 1));
        let old = code_at_step(RFC_SECRET, t - 2);
        assert_eq!(
            verify(RFC_SECRET, &old, now, None),
            None,
            "außerhalb des Fensters"
        );
        assert_eq!(verify(RFC_SECRET, "12345", now, None), None);
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        assert_eq!(verify(RFC_SECRET, &spaced, now, None), Some(t as i64));
    }

    #[test]
    fn otpauth_and_qr() {
        let url = otpauth_url(RFC_SECRET, "anna maria");
        assert!(
            url.starts_with("otpauth://totp/xlrx-drive:anna%20maria?secret=GEZDGNBV"),
            "{url}"
        );
        assert!(qr_svg(&url).contains("<svg"));
    }
}
