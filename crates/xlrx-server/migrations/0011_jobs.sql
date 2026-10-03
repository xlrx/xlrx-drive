-- Background work (PLAN 6.6): text extraction now; image analysis and embeddings later.
-- A worker leases a job by moving run_after into the future and deletes it when done, so the job
-- of a worker that crashed comes back once the lease runs out. Jobs that keep failing stay as
-- 'failed' (dead letter, shown to the admin); jobs needing a tool that is not set up wait as
-- 'waiting' until the server starts with it.
CREATE TABLE jobs (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind        text NOT NULL,
    -- What the job is about, e.g. a content hash in hex.
    key         text NOT NULL,
    -- Higher first: recently changed files before the first run over everything.
    priority    smallint NOT NULL DEFAULT 0,
    state       text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'waiting', 'failed')),
    attempts    integer NOT NULL DEFAULT 0,
    run_after   timestamptz NOT NULL DEFAULT now(),
    last_error  text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (kind, key)
);
CREATE INDEX jobs_due ON jobs (priority DESC, run_after) WHERE state = 'queued';
