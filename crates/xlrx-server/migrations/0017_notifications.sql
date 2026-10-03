-- Notifications (PLAN 8.4): the bell in the web app. Push to devices and e-mail come later.
CREATE TABLE notifications (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id        bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    -- 'shared': something was shared with the person; 'link_upload': files arrived through a
    -- file request link of theirs (merged per link while unread, for an hour).
    kind           text NOT NULL CHECK (kind IN ('shared', 'link_upload')),
    actor_user_id  bigint REFERENCES users ON DELETE SET NULL,
    node_id        bigint NOT NULL REFERENCES nodes ON DELETE CASCADE,
    details        jsonb NOT NULL DEFAULT '{}',
    created_at     timestamptz NOT NULL DEFAULT now(),
    read_at        timestamptz
);
CREATE INDEX notifications_user ON notifications (user_id, created_at DESC);
CREATE INDEX notifications_unread ON notifications (user_id) WHERE read_at IS NULL;
