-- Looking files up by content: dedup for sync clients, cleaning up thumbnails.
CREATE INDEX nodes_content_hash ON nodes (content_hash) WHERE content_hash IS NOT NULL;
