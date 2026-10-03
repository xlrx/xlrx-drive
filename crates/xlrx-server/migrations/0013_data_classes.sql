-- Data classes (PLAN 7.4): per folder "cloud" (Cloud erlaubt) or "local" (Nur lokal), inherited
-- downwards. Only explicit settings are stored; a node takes the nearest setting above it, or the
-- server's default ("local" unless configured otherwise: safe until actively allowed). Every
-- cloud path (AI analysis, outside cache, backup in the clear) must ask for it.
CREATE TABLE data_classes (
    node_id  bigint PRIMARY KEY REFERENCES nodes ON DELETE CASCADE,
    class    text NOT NULL CHECK (class IN ('cloud', 'local')),
    set_by   bigint REFERENCES users ON DELETE SET NULL,
    set_at   timestamptz NOT NULL DEFAULT now()
);
