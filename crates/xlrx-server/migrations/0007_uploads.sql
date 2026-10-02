-- Uploads in parts (PLAN 5.3): resumable, every part stored durably before it is confirmed, and
-- only a complete upload whose content checks out becomes a file.

CREATE TABLE uploads (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    size        bigint NOT NULL CHECK (size >= 0),
    -- Where the content goes: a new file, new content for a file, or content for a sync operation.
    target      jsonb NOT NULL,
    mtime_ms    bigint,
    -- Byte ranges that arrived (and were written to disk).
    received    int8multirange NOT NULL DEFAULT '{}',
    -- open → committing (being written into place) → committed (result kept for retries).
    state       text NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'committing', 'committed')),
    result      jsonb,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX uploads_user ON uploads (user_id);
