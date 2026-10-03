-- Start page suggestions (PLAN 8.1, 8.2).

-- What a person opened or downloaded: private, only for their own start page. Sources: the web
-- app, later iOS and the Mac (which reports what was opened locally in other apps).
CREATE TABLE access_events (
    user_id  bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    node_id  bigint NOT NULL REFERENCES nodes ON DELETE CASCADE,
    kind     text NOT NULL CHECK (kind IN ('open', 'download')),
    source   text NOT NULL CHECK (source IN ('web', 'ios', 'mac')),
    at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX access_events_user ON access_events (user_id, at DESC);
CREATE INDEX access_events_node ON access_events (node_id);

-- Suggestions shown and opened, to tune the weights later.
CREATE TABLE suggestion_log (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id    bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    node_id    bigint NOT NULL REFERENCES nodes ON DELETE CASCADE,
    reason     text NOT NULL,
    score      real NOT NULL,
    shown_at   timestamptz NOT NULL DEFAULT now(),
    opened_at  timestamptz
);
CREATE INDEX suggestion_log_user ON suggestion_log (user_id, node_id, shown_at DESC);
