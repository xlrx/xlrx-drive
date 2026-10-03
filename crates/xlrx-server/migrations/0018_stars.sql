-- "Markiert" (PLAN 8.2): items a person starred, for their start page. Private.
CREATE TABLE stars (
    user_id     bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    node_id     bigint NOT NULL REFERENCES nodes ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, node_id)
);
