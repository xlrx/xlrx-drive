-- Picture descriptions in the lexical search (PLAN 7.2): every change to `ai_vision` – a new
-- description, one deleted because its content became "Nur lokal" – gets a number here, written
-- under the AI lock, so the search index follows them in order and drops revoked text as well.
CREATE SEQUENCE ai_vision_seq;
CREATE TABLE ai_vision_log (
    seq           bigint PRIMARY KEY DEFAULT nextval('ai_vision_seq'),
    content_hash  bytea NOT NULL,
    at            timestamptz NOT NULL DEFAULT now()
);
