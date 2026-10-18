-- The lab store (labs-store design §1.2): what the Audio, Image and Decision
-- labs keep. One click is a group (the form, the plan of cells, the override);
-- each dispatch to a `/v1` route is a run, its request and answer kept as the
-- text that went and came; media bytes are artifacts, files under
-- `<data_dir>/labs/artifacts/<sha256>` that a group's inputs or a run's
-- outputs name. Nothing here is a second request log: a run keeps what the log
-- deliberately does not.
--
-- The `lab` CHECK names all three labs now, so the Audio and Image labs move
-- onto the store without a table rebuild.
CREATE TABLE lab_groups (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    lab         TEXT NOT NULL CHECK (lab IN ('decision', 'image', 'audio')),
    title       TEXT NOT NULL,
    form        TEXT NOT NULL,
    plan        TEXT NOT NULL,
    override    TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX idx_lab_groups_lab ON lab_groups (lab, id DESC);

CREATE TABLE lab_runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id        INTEGER NOT NULL REFERENCES lab_groups(id) ON DELETE CASCADE,
    cell            INTEGER NOT NULL,
    variant         TEXT NOT NULL,
    endpoint        TEXT NOT NULL,
    model_asked     TEXT NOT NULL,
    answered_by     TEXT NOT NULL,
    fallback_reason TEXT,
    upstream_model  TEXT,
    http_status     INTEGER NOT NULL,
    error_code      TEXT,
    error_message   TEXT,
    request         TEXT NOT NULL,
    response        TEXT,
    response_type   TEXT,
    headers         TEXT NOT NULL DEFAULT '{}',
    input_tokens    INTEGER,
    output_tokens   INTEGER,
    latency_ms      INTEGER NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (group_id, cell)
);

CREATE TABLE lab_artifacts (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id    INTEGER NOT NULL REFERENCES lab_groups(id) ON DELETE CASCADE,
    run_id      INTEGER REFERENCES lab_runs(id) ON DELETE CASCADE,
    role        TEXT NOT NULL CHECK (role IN ('input', 'output')),
    ord         INTEGER NOT NULL,
    name        TEXT NOT NULL,
    mime        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    sha256      TEXT NOT NULL,
    -- An input that came as a data URL: its head as typed, everything before
    -- the comma (`data:image/x-receipt;name=a.png;base64`), so a cell sends
    -- the URL back byte for byte. `mime` stays the sniffed type. NULL for
    -- bytes that came otherwise.
    data_url_head TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX idx_lab_artifacts_group ON lab_artifacts (group_id);
CREATE INDEX idx_lab_artifacts_run ON lab_artifacts (run_id);
CREATE INDEX idx_lab_artifacts_sha ON lab_artifacts (sha256);
