//! Extracted texts (`content_text`): one per content, with its language.

use sqlx::PgPool;

use crate::error::ApiResult;

/// Advisory lock held while a text gets its sequence number: numbers become visible in order, so
/// the indexer's cursor never skips a text that commits late.
const TEXT_LOCK: i64 = 0x786c_7278_7478_7400;

/// Most characters kept per content. Longer texts (whole books, logs, exports) are cut; their
/// beginning is what people search for anyway.
pub const MAX_CHARS: usize = 1_000_000;

/// Characters looked at to tell the language (enough for a reliable guess, cheap on long texts).
const SAMPLE_CHARS: usize = 20_000;

/// Language of a text as ISO 639-3 code, if it can be told reliably.
pub fn language(text: &str) -> Option<&'static str> {
    let sample = match text.char_indices().nth(SAMPLE_CHARS) {
        Some((i, _)) => &text[..i],
        None => text,
    };
    let info = whatlang::detect(sample)?;
    info.is_reliable().then(|| info.lang().code())
}

/// Stores the text of a content (replacing an earlier, different one) for the index to pick up.
pub async fn store(db: &PgPool, hash: &[u8; 32], source: &str, text: &str) -> ApiResult<()> {
    let (text, truncated) = match text.char_indices().nth(MAX_CHARS) {
        Some((i, _)) => (&text[..i], true),
        None => (text, false),
    };
    // Postgres text cannot hold NUL characters.
    let text = text.replace('\0', " ");
    let lang = language(&text);
    let mut tx = db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(TEXT_LOCK)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO content_text (hash, lang, source, text, truncated, seq)
         VALUES ($1, $2, $3, $4, $5, nextval('content_text_seq'))
         ON CONFLICT (hash) DO UPDATE
            SET lang = EXCLUDED.lang, source = EXCLUDED.source, text = EXCLUDED.text,
                truncated = EXCLUDED.truncated, seq = EXCLUDED.seq, created_at = now()
          -- The same text again changes nothing (no new indexing, no new embedding).
          WHERE (content_text.text, content_text.source) IS DISTINCT FROM (EXCLUDED.text, EXCLUDED.source)",
    )
    .bind(hash.to_vec())
    .bind(lang)
    .bind(source)
    .bind(&text)
    .bind(truncated)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_german_from_english() {
        assert_eq!(
            language(
                "Die Rechnung für die Heizung ist am Montag gekommen und muss bis Ende des Monats bezahlt werden."
            ),
            Some("deu")
        );
        assert_eq!(
            language(
                "The invoice for the heating arrived on Monday and has to be paid by the end of the month."
            ),
            Some("eng")
        );
        assert_eq!(language("1234 5678"), None);
    }
}
