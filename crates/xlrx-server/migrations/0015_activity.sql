-- Activity (PLAN 8): what happened to files and folders, shown to everyone who may see them.
-- Changes to files come from the journal itself (who, what, when); this adds what the journal
-- does not know.

-- The name before a rename ("„Alt“ in „Neu“ umbenannt").
ALTER TABLE journal ADD COLUMN prev_name text;
-- History of a single item.
CREATE INDEX journal_node ON journal (node_id, seq DESC);

-- When the first import of a root finished: what a scan found before that is not news.
ALTER TABLE roots ADD COLUMN first_scanned_at timestamptz;
UPDATE roots SET first_scanned_at = scanned_at;

-- Sharing and public links.
CREATE TABLE events (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at             timestamptz NOT NULL DEFAULT now(),
    -- NULL: someone through a public link.
    actor_user_id  bigint REFERENCES users ON DELETE SET NULL,
    node_id        bigint NOT NULL REFERENCES nodes ON DELETE CASCADE,
    kind           text NOT NULL CHECK (kind IN ('shared', 'link_created', 'link_download',
                                                 'link_upload', 'link_edit')),
    details        jsonb NOT NULL DEFAULT '{}'
);
CREATE INDEX events_node ON events (node_id, at DESC);
CREATE INDEX events_at ON events (at DESC);
