-- Sharing (PLAN 9): groups, members of shared roots ("Geteilte Ablagen"), shares on folders and
-- files. Rights are inherited downwards: a share on a folder covers everything below it.
--
-- Roles: 'viewer' (Ansehen), 'editor' (Bearbeiten: change the content), 'manager' (Verwalten:
-- also share further). The owner of a home root has every right there.
-- A principal is either a person or a group: exactly one of user_id / group_id is set.

CREATE TABLE groups (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name         text NOT NULL,
    name_folded  text NOT NULL UNIQUE,
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE group_members (
    group_id  bigint NOT NULL REFERENCES groups ON DELETE CASCADE,
    user_id   bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    PRIMARY KEY (group_id, user_id)
);
CREATE INDEX group_members_user ON group_members (user_id);

-- Who may use a shared root and how. Homes have no members: only their owner.
CREATE TABLE root_members (
    root_id   bigint NOT NULL REFERENCES roots ON DELETE CASCADE,
    user_id   bigint REFERENCES users ON DELETE CASCADE,
    group_id  bigint REFERENCES groups ON DELETE CASCADE,
    role      text NOT NULL CHECK (role IN ('viewer', 'editor', 'manager')),
    CHECK ((user_id IS NULL) <> (group_id IS NULL)),
    UNIQUE NULLS NOT DISTINCT (root_id, user_id, group_id)
);
CREATE INDEX root_members_user ON root_members (user_id) WHERE user_id IS NOT NULL;
CREATE INDEX root_members_group ON root_members (group_id) WHERE group_id IS NOT NULL;

CREATE TABLE shares (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    node_id     bigint NOT NULL REFERENCES nodes ON DELETE CASCADE,
    user_id     bigint REFERENCES users ON DELETE CASCADE,
    group_id    bigint REFERENCES groups ON DELETE CASCADE,
    role        text NOT NULL CHECK (role IN ('viewer', 'editor', 'manager')),
    -- After this moment the share no longer counts (it stays listed until removed).
    expires_at  timestamptz,
    created_by  bigint REFERENCES users ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CHECK ((user_id IS NULL) <> (group_id IS NULL)),
    UNIQUE NULLS NOT DISTINCT (node_id, user_id, group_id)
);
CREATE INDEX shares_node ON shares (node_id);
CREATE INDEX shares_user ON shares (user_id) WHERE user_id IS NOT NULL;
CREATE INDEX shares_group ON shares (group_id) WHERE group_id IS NOT NULL;
