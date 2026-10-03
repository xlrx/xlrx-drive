//! The search syntax (PLAN 6.5): words, „Phrasen“, `-ausschluss`, `typ:`, `in:`, `nach:`, `vor:`,
//! `dokument:`.
//!
//! Our own small parser instead of Tantivy's query language: people cannot address internal
//! fields, and every input is valid except a few named mistakes (an unreadable date).

use time::{Date, Month};

/// A word or phrase as typed.
#[derive(Clone, Debug, PartialEq)]
pub struct Clause {
    pub text: String,
    /// In quotes: exact words only (no prefixes, no similar spellings).
    pub quoted: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Parsed {
    pub clauses: Vec<Clause>,
    pub excluded: Vec<Clause>,
    /// `typ:` values (category or extension).
    pub types: Vec<String>,
    pub not_types: Vec<String>,
    /// `in:` folder names.
    pub folders: Vec<String>,
    pub not_folders: Vec<String>,
    /// `nach:` – modified at or after (seconds since the epoch).
    pub after: Option<i64>,
    /// `vor:` – modified before.
    pub before: Option<i64>,
    /// `dokument:` – kind of document recognized in a picture (`rechnung`, `vertrag` …).
    pub docs: Vec<String>,
    pub not_docs: Vec<String>,
}

impl Parsed {
    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty() && !self.has_filters()
    }

    /// Anything besides words and phrases to look for (exclusions, kinds, folders, dates).
    pub fn has_filters(&self) -> bool {
        !(self.excluded.is_empty()
            && self.types.is_empty()
            && self.not_types.is_empty()
            && self.folders.is_empty()
            && self.not_folders.is_empty()
            && self.after.is_none()
            && self.before.is_none()
            && self.docs.is_empty()
            && self.not_docs.is_empty())
    }
}

/// Longest query accepted (characters).
pub const MAX_LEN: usize = 500;
/// Most words, phrases and filters in one query.
const MAX_PARTS: usize = 24;

fn opens_quote(c: char) -> bool {
    matches!(c, '"' | '„' | '“' | '”' | '»' | '«')
}

fn closes_quote(c: char) -> bool {
    matches!(c, '"' | '“' | '”' | '«' | '»')
}

#[derive(Clone, Copy, PartialEq)]
enum Key {
    Type,
    In,
    After,
    Before,
    Doc,
}

fn key(word: &str) -> Option<Key> {
    Some(match word.to_lowercase().as_str() {
        "typ" | "type" => Key::Type,
        "in" => Key::In,
        "nach" | "after" | "ab" => Key::After,
        "vor" | "before" => Key::Before,
        "dokument" | "doc" => Key::Doc,
        _ => return None,
    })
}

pub fn parse(q: &str) -> Result<Parsed, String> {
    if q.chars().count() > MAX_LEN {
        return Err("Die Suche ist zu lang.".into());
    }
    let mut out = Parsed::default();
    let mut parts = 0;
    let mut chars = q.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else { break };
        let negated = first == '-';
        if negated {
            chars.next();
            // A lone dash is just a dash.
            if chars.peek().is_none_or(|c| c.is_whitespace()) {
                continue;
            }
        }
        let quoted_value = |chars: &mut std::iter::Peekable<std::str::Chars>| {
            chars.next();
            let mut v = String::new();
            for c in chars.by_ref() {
                if closes_quote(c) {
                    break;
                }
                v.push(c);
            }
            v
        };
        if chars.peek().is_some_and(|&c| opens_quote(c)) {
            let text = quoted_value(&mut chars);
            push_clause(&mut out, negated, text, true);
            parts += 1;
        } else {
            let mut word = String::new();
            let mut op = None;
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                chars.next();
                if c == ':' && op.is_none() && !word.is_empty() {
                    if let Some(k) = key(&word) {
                        op = Some(k);
                        word.clear();
                        if chars.peek().is_some_and(|&c| opens_quote(c)) {
                            word = quoted_value(&mut chars);
                            break;
                        }
                        continue;
                    }
                }
                word.push(c);
            }
            let value = word.trim().to_string();
            match op {
                None => push_clause(&mut out, negated, value, false),
                Some(_) if value.is_empty() => {}
                Some(Key::Type) => {
                    let v = value.trim_start_matches('.').to_lowercase();
                    if negated {
                        out.not_types.push(v);
                    } else {
                        out.types.push(v);
                    }
                }
                Some(Key::In) if negated => out.not_folders.push(value),
                Some(Key::In) => out.folders.push(value),
                Some(Key::Doc) if negated => out.not_docs.push(value.to_lowercase()),
                Some(Key::Doc) => out.docs.push(value.to_lowercase()),
                Some(Key::After) => out.after = Some(date(&value, "nach")?),
                Some(Key::Before) => out.before = Some(date(&value, "vor")?),
            }
            parts += 1;
        }
        if parts > MAX_PARTS {
            return Err("Zu viele Suchbegriffe.".into());
        }
    }
    Ok(out)
}

fn push_clause(out: &mut Parsed, negated: bool, text: String, quoted: bool) {
    if text.trim().is_empty() {
        return;
    }
    let c = Clause {
        text: text.trim().to_string(),
        quoted,
    };
    if negated {
        out.excluded.push(c);
    } else {
        out.clauses.push(c);
    }
}

/// Start of the day, month or year given (UTC): `2025`, `2025-03`, `2025-03-01`, `1.3.2025`,
/// `3.2025`.
fn date(v: &str, op: &str) -> Result<i64, String> {
    let bad = || format!("Unbekanntes Datum bei {op}: „{v}“ – zum Beispiel {op}:2025-03-01.");
    let num = |s: &str| s.parse::<i32>().map_err(|_| bad());
    let (y, m, d) = if v.contains('.') {
        let p: Vec<&str> = v.split('.').collect();
        match p.as_slice() {
            [d, m, y] => (num(y)?, num(m)?, num(d)?),
            [m, y] => (num(y)?, num(m)?, 1),
            _ => return Err(bad()),
        }
    } else {
        let p: Vec<&str> = v.split('-').collect();
        match p.as_slice() {
            [y] => (num(y)?, 1, 1),
            [y, m] => (num(y)?, num(m)?, 1),
            [y, m, d] => (num(y)?, num(m)?, num(d)?),
            _ => return Err(bad()),
        }
    };
    if !(1900..=2200).contains(&y) {
        return Err(bad());
    }
    let month = u8::try_from(m)
        .ok()
        .and_then(|m| Month::try_from(m).ok())
        .ok_or_else(bad)?;
    let day = u8::try_from(d).map_err(|_| bad())?;
    let date = Date::from_calendar_date(y, month, day).map_err(|_| bad())?;
    Ok(date.midnight().assume_utc().unix_timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clause(t: &str, quoted: bool) -> Clause {
        Clause {
            text: t.into(),
            quoted,
        }
    }

    #[test]
    fn words_phrases_exclusions() {
        let p = parse("Rechnung „Heizung Keller“ -Entwurf -\"alte Version\" - x").unwrap();
        assert_eq!(
            p.clauses,
            [
                clause("Rechnung", false),
                clause("Heizung Keller", true),
                clause("x", false)
            ]
        );
        assert_eq!(
            p.excluded,
            [clause("Entwurf", false), clause("alte Version", true)]
        );
    }

    #[test]
    fn filters() {
        let p =
            parse("typ:PDF -typ:.docx in:\"Steuer 2026\" -in:Alt nach:2025 vor:1.3.2026 Heizung")
                .unwrap();
        assert_eq!(p.types, ["pdf"]);
        assert_eq!(p.not_types, ["docx"]);
        assert_eq!(p.folders, ["Steuer 2026"]);
        assert_eq!(p.not_folders, ["Alt"]);
        assert_eq!(p.after, Some(1_735_689_600));
        assert_eq!(p.before, Some(1_772_323_200));
        assert_eq!(p.clauses, [clause("Heizung", false)]);
        let p = parse("dokument:Rechnung -dokument:brief Heizung").unwrap();
        assert_eq!(
            (p.docs, p.not_docs),
            (vec!["rechnung".to_string()], vec!["brief".to_string()])
        );
        assert!(parse("dokument:vertrag").unwrap().has_filters());
    }

    #[test]
    fn colons_in_ordinary_words_stay_words() {
        let p = parse("Re:Angebot 10:30 typ:").unwrap();
        assert_eq!(
            p.clauses,
            [clause("Re:Angebot", false), clause("10:30", false)]
        );
        assert!(p.types.is_empty());
    }

    #[test]
    fn mistakes_are_named() {
        assert!(parse("nach:gestern").unwrap_err().contains("nach:"));
        assert!(parse("vor:2025-13-01").is_err());
        assert!(parse(&"a ".repeat(30)).is_err());
        assert!(parse(&"a".repeat(600)).is_err());
        assert_eq!(parse("   ").unwrap(), Parsed::default());
    }

    #[test]
    fn unclosed_quote_runs_to_the_end() {
        let p = parse("\"offene Phrase").unwrap();
        assert_eq!(p.clauses, [clause("offene Phrase", true)]);
    }
}
