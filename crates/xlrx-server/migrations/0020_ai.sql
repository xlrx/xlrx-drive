-- AI search (PLAN 6.3, 7): vectors per content in pgvector, picture descriptions, costs.
-- Everything here is derived from file contents and can be made again; results of the cloud are
-- removed as soon as a content may no longer leave the house.
CREATE EXTENSION IF NOT EXISTS vector;

-- Vectors of a content in one space: 'cloud' (the provider's model, "Cloud erlaubt"), 'local'
-- (the text model in the home network) or 'clip' (the picture model in the home network). Each
-- model gets its own partial index (created by the server for the configured models), so models
-- of different sizes live side by side, also while one replaces another.
CREATE TABLE ai_vectors (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    content_hash  bytea NOT NULL,
    space         text NOT NULL CHECK (space IN ('cloud', 'local', 'clip')),
    model         text NOT NULL,
    -- 'text': a piece of the extracted text; 'description': what a picture shows; 'image': the
    -- picture itself (CLIP).
    source        text NOT NULL CHECK (source IN ('text', 'description', 'image')),
    -- Where the piece starts in the text and how long it is (characters), for snippets.
    start         integer NOT NULL DEFAULT 0,
    len           integer NOT NULL DEFAULT 0,
    vec           halfvec NOT NULL
);
CREATE INDEX ai_vectors_content ON ai_vectors (content_hash, space);

-- What was done for a content in a space, with which model and which version of its text: nothing
-- is sent twice.
CREATE TABLE ai_done (
    content_hash  bytea NOT NULL,
    space         text NOT NULL CHECK (space IN ('cloud', 'local', 'clip')),
    task          text NOT NULL CHECK (task IN ('text', 'image')),
    model         text NOT NULL,
    -- content_text.seq of the text embedded; 0 for pictures.
    version       bigint NOT NULL DEFAULT 0,
    at            timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (content_hash, space, task)
);

-- One vision call per picture ("Cloud erlaubt" only): what it shows (PLAN 7.2).
CREATE TABLE ai_vision (
    content_hash   bytea PRIMARY KEY,
    model          text NOT NULL,
    description    text NOT NULL,
    tags           text[] NOT NULL DEFAULT '{}',
    text_in_image  text NOT NULL DEFAULT '',
    doc_type       text,
    date_found     date,
    at             timestamptz NOT NULL DEFAULT now()
);

-- Calls, tokens and costs per month and kind ('embed', 'vision'), for the budget (PLAN 7.3).
CREATE TABLE ai_usage (
    month       date NOT NULL,
    kind        text NOT NULL,
    calls       bigint NOT NULL DEFAULT 0,
    tokens_in   bigint NOT NULL DEFAULT 0,
    tokens_out  bigint NOT NULL DEFAULT 0,
    cost        double precision NOT NULL DEFAULT 0,
    PRIMARY KEY (month, kind)
);

-- The pipeline: how far it followed texts and journal; whether the cloud analysis runs. It is off
-- until an administrator starts it (it costs money); a trial run handles this many contents and
-- stops again. The monthly budget set in the administration (else the configured one).
CREATE TABLE ai_state (
    id           boolean PRIMARY KEY DEFAULT true CHECK (id),
    text_seq     bigint NOT NULL DEFAULT 0,
    journal_seq  bigint NOT NULL DEFAULT 0,
    cloud_on     boolean NOT NULL DEFAULT false,
    trial_left   integer,
    budget       double precision
);
INSERT INTO ai_state DEFAULT VALUES;
