-- Sync clients send every change with an operation id that stays the same across retries
-- (PLAN 5.3). The result is kept, so a repeated request gets the same answer instead of being
-- executed twice.
CREATE TABLE sync_ops (
    user_id     bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    device      text NOT NULL,
    op_id       bigint NOT NULL,
    result      jsonb NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, device, op_id)
);
