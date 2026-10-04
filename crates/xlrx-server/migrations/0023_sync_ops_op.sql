-- Sync operations (ADR 0002): the operation is kept next to its result. The same
-- (user, device, op_id) with a different operation (a client whose state was restored, cloned or
-- reinstalled) is refused with 409 op_mismatch instead of getting a stranger's result. NULL for
-- rows written before: those answer as before.
ALTER TABLE sync_ops ADD COLUMN op jsonb;
