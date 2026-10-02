-- Files (PLAN 4, 5.1, 13.3): roots, nodes mirroring plain files on disk, journal, versions.

-- A root is a directory on the NAS that xlrx manages: a person's "My Drive" (home) or, from M3 on,
-- a shared space. The path is relative to XLRX_DATA_DIR (the /volume1 mount).
CREATE TABLE roots (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind           text NOT NULL CHECK (kind IN ('home', 'space')),
    name           text NOT NULL,
    rel_path       text NOT NULL UNIQUE,
    owner_user_id  bigint REFERENCES users ON DELETE SET NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    -- Last completed reconciliation scan.
    scanned_at     timestamptz
);
CREATE UNIQUE INDEX roots_home ON roots (owner_user_id) WHERE kind = 'home';

-- One row per file or directory. Paths are derived from parent_id + name, so renaming and moving
-- are O(1). The root directory itself is a node with parent_id NULL.
CREATE TABLE nodes (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    root_id       bigint NOT NULL REFERENCES roots ON DELETE CASCADE,
    parent_id     bigint REFERENCES nodes,
    name          text NOT NULL,
    name_folded   text NOT NULL,
    kind          text NOT NULL CHECK (kind IN ('file', 'dir')),
    -- Files only: content (schema xlrx-content-v1) and size.
    content_hash  bytea,
    size          bigint,
    -- Journal sequence of the last content change (files) or of the creation (directories).
    -- Clients use it as base revision for uploads and deletes.
    rev           bigint NOT NULL,
    -- Journal sequence of the last change of any kind.
    seq           bigint NOT NULL,
    mtime         timestamptz,
    -- Identity and fingerprint on disk, to detect external changes (SMB, File Station) and moves.
    fs_dev        bigint,
    fs_ino        bigint,
    fs_size       bigint,
    fs_mtime_ns   bigint,
    fs_ctime_ns   bigint,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    -- Deleted nodes stay as rows: trash, journal and versions refer to them.
    deleted_at    timestamptz,
    deleted_by    bigint REFERENCES users ON DELETE SET NULL,
    -- Where a node deleted through xlrx lies in the trash (relative to the state directory).
    -- NULL for external deletions (only snapshots can bring those back).
    trash_path    text
);
CREATE INDEX nodes_children ON nodes (parent_id, name_folded) WHERE deleted_at IS NULL;
CREATE INDEX nodes_root ON nodes (root_id) WHERE deleted_at IS NULL;
CREATE INDEX nodes_inode ON nodes (root_id, fs_dev, fs_ino) WHERE deleted_at IS NULL;
CREATE INDEX nodes_seq ON nodes (seq);
CREATE INDEX nodes_trash ON nodes (root_id, deleted_at) WHERE trash_path IS NOT NULL;

-- Every change gets a monotonic sequence number. Clients fetch "everything after my cursor".
-- Writers serialize on an advisory lock, so sequence order equals commit order and no client can
-- skip a change that commits late.
CREATE SEQUENCE journal_seq;
CREATE TABLE journal (
    seq            bigint PRIMARY KEY DEFAULT nextval('journal_seq'),
    root_id        bigint NOT NULL REFERENCES roots ON DELETE CASCADE,
    node_id        bigint NOT NULL REFERENCES nodes ON DELETE CASCADE,
    op             text NOT NULL CHECK (op IN ('create', 'update', 'move', 'delete', 'restore')),
    -- 'api' (through xlrx, with actor) or 'scan' (found on disk: SMB, File Station, Synology Drive …).
    source         text NOT NULL CHECK (source IN ('api', 'scan')),
    actor_user_id  bigint REFERENCES users ON DELETE SET NULL,
    at             timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX journal_root_seq ON journal (root_id, seq);

-- Earlier contents of a file, kept in the content-addressed version store.
CREATE TABLE versions (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    node_id       bigint NOT NULL REFERENCES nodes ON DELETE CASCADE,
    rev           bigint NOT NULL,
    content_hash  bytea NOT NULL,
    size          bigint NOT NULL,
    mtime         timestamptz,
    -- Relative to the state directory, e.g. store/versions/ab/cd/<hash>.
    store_path    text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    created_by    bigint REFERENCES users ON DELETE SET NULL
);
CREATE INDEX versions_node ON versions (node_id, rev DESC);
CREATE INDEX versions_hash ON versions (content_hash);

-- Intent log (PLAN 4.3): written before a multi-step change on disk, removed after the database
-- transaction. On startup, leftovers are completed or rolled back.
CREATE TABLE pending_ops (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind        text NOT NULL,
    payload     jsonb NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
