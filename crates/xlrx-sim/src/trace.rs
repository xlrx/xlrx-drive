//! Fingerprint of a run: a hash over every trace line, so that a change to the engine or the
//! simulator that changes behaviour (and only such a change) shows up as a different hash for the
//! same seed. The golden files in `tests/golden/` hold these hashes for the seed tests.
//!
//! FNV-1a 64: simple, fixed and dependency-free; `std`'s `DefaultHasher` does not promise a stable
//! algorithm across Rust versions.

const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Copy, Debug)]
pub(crate) struct TraceHash(u64);

impl Default for TraceHash {
    fn default() -> Self {
        Self(OFFSET)
    }
}

impl TraceHash {
    /// Adds one trace line (and a line end, so that "ab" + "c" differs from "a" + "bc").
    pub(crate) fn line(&mut self, s: &str) {
        for b in s.bytes().chain(std::iter::once(b'\n')) {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(PRIME);
        }
    }

    pub(crate) fn value(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_bekannte_werte() {
        // FNV-1a 64 of "\n" and of "a\n", computed independently of this code.
        let mut h = TraceHash::default();
        h.line("");
        assert_eq!(h.value(), 0xaf63_c74c_8601_c8dd);
        let mut h = TraceHash::default();
        h.line("a");
        assert_eq!(h.value(), 0x089b_dc07_b544_e7b2);
    }

    #[test]
    fn zeilengrenzen_zaehlen() {
        let mut a = TraceHash::default();
        a.line("ab");
        a.line("c");
        let mut b = TraceHash::default();
        b.line("a");
        b.line("bc");
        assert_ne!(a.value(), b.value());
    }
}
