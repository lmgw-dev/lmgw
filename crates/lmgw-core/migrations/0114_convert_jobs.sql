-- Async document-conversion jobs and what callers send (document-conversion
-- design §4.4, §7.3, §7.6, §7.8; WP5).
--
-- `convert_jobs`: one row per async request (the native route's
-- `async: true`, and every docling-serve async or batch request). Its
-- files' results are `<data dir>/convert/<id>/<index>/entry.json`, never in
-- a row; the rows hold states, counts and lmgw's own notes. `key_id` is the
-- key that made it (NULL: a request without a key); another key's request
-- for it is a 404. `origin` is `native` or `docling`, and `adapter` holds
-- the docling request's answer settings (outputs, target, option notes) as
-- JSON. `fetched_at` is a docling client's first result fetch: the job goes
-- 300 s later (docling-serve's single-use results). A row still `running`
-- at start is `interrupted` (the process that ran it is gone).
CREATE TABLE convert_jobs (
    id           TEXT PRIMARY KEY,
    key_id       INTEGER,
    origin       TEXT NOT NULL CHECK (origin IN ('native', 'docling')),
    state        TEXT NOT NULL CHECK (state IN
                     ('running', 'succeeded', 'failed', 'cancelled', 'interrupted')),
    created_at   TEXT NOT NULL,
    finished_at  TEXT,
    fetched_at   TEXT,
    files        INTEGER NOT NULL,
    input_bytes  INTEGER NOT NULL,
    parts        INTEGER NOT NULL DEFAULT 0,
    parts_done   INTEGER NOT NULL DEFAULT 0,
    error        TEXT,
    warnings     TEXT NOT NULL DEFAULT '[]',
    tools        TEXT NOT NULL DEFAULT '[]',
    adapter      TEXT
);
CREATE INDEX convert_jobs_state ON convert_jobs (state, finished_at);

CREATE TABLE convert_job_files (
    job_id      TEXT NOT NULL REFERENCES convert_jobs (id) ON DELETE CASCADE,
    idx         INTEGER NOT NULL,
    name        TEXT NOT NULL,
    bytes       INTEGER NOT NULL,
    state       TEXT NOT NULL CHECK (state IN
                    ('pending', 'converted', 'partial', 'failed', 'skipped',
                     'cancelled', 'interrupted')),
    parts       INTEGER NOT NULL DEFAULT 0,
    parts_done  INTEGER NOT NULL DEFAULT 0,
    error       TEXT,
    PRIMARY KEY (job_id, idx)
);

-- What a conversion request sent (§7.8, D17), beside its request-log row and
-- gone with it (the log's retention): the route, the file count, the target,
-- every option it set with its outcome, and the hosts of its `http` sources.
-- Option values only for options whose values are a small fixed set; free
-- text and addresses as `set`. No file names, no content, no URLs, no
-- headers.
CREATE TABLE convert_calls (
    log_id   INTEGER PRIMARY KEY REFERENCES request_logs (id) ON DELETE CASCADE,
    route    TEXT NOT NULL,
    files    INTEGER NOT NULL,
    target   TEXT,
    options  TEXT NOT NULL DEFAULT '[]',
    hosts    TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX convert_calls_route ON convert_calls (route);
