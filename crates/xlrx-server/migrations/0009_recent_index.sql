-- Files changed last, per root (start page).
CREATE INDEX nodes_recent ON nodes (root_id, mtime DESC) WHERE kind = 'file' AND deleted_at IS NULL;
