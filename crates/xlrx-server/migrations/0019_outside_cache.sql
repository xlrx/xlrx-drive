-- Outside cache (PLAN 15.2): contents of "Cloud erlaubt" folders mirrored to a private S3 bucket and
-- served from there to people outside the home network. Content-addressed: one object per content.
CREATE TABLE s3_objects (
    content_hash  bytea PRIMARY KEY,
    -- Keyed hash of the content: the bucket sees neither names nor matchable hashes.
    key           text NOT NULL UNIQUE,
    size          bigint NOT NULL,
    -- 'uploading': being sent; 'ready': may be served; 'deleting': to be removed from the bucket.
    state         text NOT NULL CHECK (state IN ('uploading', 'ready', 'deleting')),
    -- Why it was mirrored: a public link points at it, it was fetched from outside repeatedly, or
    -- it was prefetched for someone who is often away.
    reason        text NOT NULL CHECK (reason IN ('link', 'popular', 'prefetch')),
    created_at    timestamptz NOT NULL DEFAULT now(),
    uploaded_at   timestamptz,
    last_hit      timestamptz,
    hits          integer NOT NULL DEFAULT 0
);

-- Full downloads from outside the home network: what is fetched repeatedly is worth mirroring.
CREATE TABLE remote_fetches (
    content_hash  bytea NOT NULL,
    at            timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX remote_fetches_hash ON remote_fetches (content_hash, at);

-- "Unterwegs vorausladen": starred and suggested files are mirrored at night.
ALTER TABLE users ADD COLUMN prefetch boolean NOT NULL DEFAULT false;

CREATE INDEX IF NOT EXISTS versions_content ON versions (content_hash);
