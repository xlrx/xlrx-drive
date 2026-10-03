-- Public links (PLAN 9.2): a file or folder for anyone with the link, without an account.
--
-- The token itself is never stored in the clear: its SHA-256 for the lookup, and the token sealed
-- with the server key (AES-256-GCM) so it can be shown again to whoever manages the item.
-- Kinds: 'view' (Ansehen), 'download' (Herunterladen), 'upload' (Nur hochladen, folders only),
-- 'edit' (Bearbeiten: download, add files, new contents with the old ones kept as versions).
-- Removing a link deletes its row; the audit log keeps what happened.

CREATE TABLE links (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    token_hash     bytea NOT NULL UNIQUE,
    token_sealed   bytea NOT NULL,
    node_id        bigint NOT NULL REFERENCES nodes ON DELETE CASCADE,
    kind           text NOT NULL CHECK (kind IN ('view', 'download', 'upload', 'edit')),
    password_hash  text,
    expires_at     timestamptz,
    max_downloads  integer CHECK (max_downloads > 0),
    downloads      integer NOT NULL DEFAULT 0,
    created_by     bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX links_node ON links (node_id);
