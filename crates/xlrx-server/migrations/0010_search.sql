-- Full-text search (PLAN 6). The search index in the state directory is derived from nodes,
-- journal and the extracted texts below; it can be rebuilt from them at any time.

-- A move records whether the parent changed (and not just the name): the index then refreshes the
-- whole subtree, whose ancestors changed. NULL for other operations and for older entries.
ALTER TABLE journal ADD COLUMN reparented boolean;

-- Text extracted from file content, once per content (BLAKE3, xlrx-content-v1): duplicates are read
-- only once, and renaming or moving never extracts again (PLAN 6.1).
CREATE SEQUENCE content_text_seq;
CREATE TABLE content_text (
    hash        bytea PRIMARY KEY,
    -- Detected language as ISO 639-3 code ('deu', 'eng', …), NULL if unsure.
    lang        text,
    -- How the text was obtained: 'plain' (read directly), 'tika', 'ocr'.
    source      text NOT NULL,
    text        text COMPRESSION lz4 NOT NULL,
    -- The text was cut at the length limit.
    truncated   boolean NOT NULL DEFAULT false,
    -- Order of arrival: the indexer picks up new and changed texts after its cursor. Writers hold
    -- an advisory lock while assigning it, so numbers become visible in order.
    seq         bigint NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX content_text_by_seq ON content_text (seq);
