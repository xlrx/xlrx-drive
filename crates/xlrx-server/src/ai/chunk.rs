//! Pieces of a text for embedding (PLAN 6.3): about 400–500 tokens each, overlapping, cut at
//! paragraph or sentence ends where possible.

/// A piece: where it starts in the text and how long it is (in characters), and its words with
/// whitespace runs collapsed (the provider counts every character).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub start: usize,
    pub len: usize,
    pub text: String,
}

/// Fewer letters and digits than this: nothing to embed.
const MIN_LETTERS: usize = 10;

/// Does a text have enough to embed?
pub fn worth_embedding(text: &str) -> bool {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .take(MIN_LETTERS)
        .count()
        >= MIN_LETTERS
}

/// Cuts `text` into at most `max` pieces of at most `size` characters, each starting `overlap`
/// characters before the previous one ended.
pub fn split(text: &str, size: usize, overlap: usize, max: usize) -> Vec<Chunk> {
    let size = size.max(16);
    let overlap = overlap.min(size / 2);
    // Only the beginning can end up in pieces.
    let chars: Vec<char> = text.chars().take(max.saturating_mul(size) + size).collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut pos = skip_space(&chars, 0);
    while pos < n && out.len() < max {
        let mut end = (pos + size).min(n);
        if end < n {
            end = break_point(&chars, pos + size * 3 / 4, end).unwrap_or(end);
        }
        let piece: String = chars[pos..end].iter().collect();
        if worth_embedding(&piece) {
            out.push(Chunk {
                start: pos,
                len: end - pos,
                text: piece.split_whitespace().collect::<Vec<_>>().join(" "),
            });
        }
        if end >= n {
            break;
        }
        // Start the next piece a little earlier, at the beginning of a word.
        let mut next = end.saturating_sub(overlap).max(pos + 1);
        while next < end && !chars[next].is_whitespace() {
            next += 1;
        }
        pos = skip_space(&chars, next);
    }
    out
}

fn skip_space(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    i
}

/// The best place to end a piece within `[from, to)`: after a paragraph, else after a sentence,
/// else at a space.
fn break_point(chars: &[char], from: usize, to: usize) -> Option<usize> {
    let window = from..to;
    let last = |pred: &dyn Fn(usize) -> bool| window.clone().rev().find(|&i| pred(i));
    last(&|i| i > 0 && chars[i] == '\n' && chars[i - 1] == '\n')
        .or_else(|| {
            last(&|i| {
                i > 0 && chars[i].is_whitespace() && matches!(chars[i - 1], '.' | '!' | '?' | ':')
            })
        })
        .or_else(|| last(&|i| chars[i].is_whitespace()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_piece() {
        let c = split("  Rechnung   Heizung\n2025 Wartung  ", 100, 20, 3);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, "Rechnung Heizung 2025 Wartung");
        assert_eq!(c[0].start, 2);
        assert!(split("  12 ", 100, 20, 3).is_empty());
        assert!(!worth_embedding("1 2 3"));
        assert!(worth_embedding("Hallo Welt, wie geht es?"));
    }

    #[test]
    fn long_text_overlaps_and_ends_at_sentences() {
        let sentence = "Die Heizung wurde am Montag gewartet und funktioniert wieder. ";
        let text = sentence.repeat(40);
        let c = split(&text, 300, 60, 100);
        assert!(c.len() > 5);
        for (a, b) in c.iter().zip(c.iter().skip(1)) {
            // Overlap: the next piece starts before this one ends.
            assert!(b.start < a.start + a.len, "{a:?} {b:?}");
            assert!(b.start > a.start);
        }
        for p in &c[..c.len() - 1] {
            assert!(p.text.ends_with('.'), "{}", p.text);
            assert!(p.len <= 300);
        }
        // Offsets point into the text.
        let chars: Vec<char> = text.chars().collect();
        let p = &c[2];
        let raw: String = chars[p.start..p.start + p.len].iter().collect();
        assert_eq!(raw.split_whitespace().collect::<Vec<_>>().join(" "), p.text);
    }

    #[test]
    fn at_most_max_pieces() {
        let text = "wort ".repeat(10_000);
        assert_eq!(split(&text, 200, 20, 3).len(), 3);
    }

    #[test]
    fn text_without_spaces() {
        let text = "x".repeat(1000);
        let c = split(&text, 300, 50, 10);
        assert!(c.len() >= 3);
        assert_eq!(c.iter().map(|p| p.len).max(), Some(300));
    }
}
