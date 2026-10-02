-- Trash (PLAN 4.3): deleting through xlrx moves an item into the trash as a whole. Its
-- descendants are marked deleted together with it (deleted_with = the trashed item) and come
-- back with it on restore.
ALTER TABLE nodes ADD COLUMN deleted_with bigint REFERENCES nodes;
CREATE INDEX nodes_deleted_with ON nodes (deleted_with) WHERE deleted_with IS NOT NULL;
