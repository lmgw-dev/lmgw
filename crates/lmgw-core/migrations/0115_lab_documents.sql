-- no-transaction
-- The Documents lab (document-conversion design WP12) keeps its history in
-- the lab store: `lab_groups.lab` and `lab_presets.lab` gain 'documents'.
-- `lab_batches.lab` keeps its three labs, since set runs are the Decision
-- lab's only.
--
-- SQLite cannot widen a CHECK in place, so this is 0090's twelve-step table
-- rebuild, for its reasons: `-- no-transaction` only because `PRAGMA
-- foreign_keys` is a no-op inside a transaction (`lab_runs` and
-- `lab_artifacts` reference `lab_groups(id) ON DELETE CASCADE`, and a DROP
-- with foreign keys on would delete their rows; `lab_batches` references
-- `lab_presets(id)`), the rebuild itself one `BEGIN … COMMIT`, and the
-- foreign-key check in Rust after the migrator
-- (`store::migration_guards::foreign_keys`). Every column and every id is
-- copied, and the AUTOINCREMENT high-water marks are carried over, as 0090
-- does, so a deleted top id is not handed out again.
--
-- Replay-safe: a crash between `COMMIT` and sqlx's record replays the file
-- against the new tables, which it copies again unchanged.

PRAGMA foreign_keys = OFF;

BEGIN;

CREATE TABLE lab_groups_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    lab         TEXT NOT NULL CHECK (lab IN ('decision', 'image', 'audio', 'documents')),
    title       TEXT NOT NULL,
    form        TEXT NOT NULL,
    plan        TEXT NOT NULL,
    override    TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    batch_id    INTEGER REFERENCES lab_batches(id) ON DELETE CASCADE,
    item_ix     INTEGER,
    expected    TEXT,
    item_key    TEXT
);

INSERT INTO lab_groups_new
    (id, lab, title, form, plan, override, created_at, batch_id, item_ix, expected, item_key)
SELECT id, lab, title, form, plan, override, created_at, batch_id, item_ix, expected, item_key
FROM lab_groups;

INSERT INTO sqlite_sequence (name, seq)
SELECT 'lab_groups_new', 0
 WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'lab_groups_new');

UPDATE sqlite_sequence
   SET seq = MAX(seq, COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'lab_groups'), 0))
 WHERE name = 'lab_groups_new';

DROP TABLE lab_groups;
ALTER TABLE lab_groups_new RENAME TO lab_groups;
CREATE INDEX idx_lab_groups_lab ON lab_groups (lab, id DESC);
CREATE INDEX idx_lab_groups_batch ON lab_groups (batch_id, item_ix);

CREATE TABLE lab_presets_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    lab         TEXT NOT NULL CHECK (lab IN ('decision', 'image', 'audio', 'documents')),
    kind        TEXT NOT NULL,
    name        TEXT NOT NULL,
    body        TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (lab, kind, name)
);

INSERT INTO lab_presets_new (id, lab, kind, name, body, created_at, updated_at)
SELECT id, lab, kind, name, body, created_at, updated_at
FROM lab_presets;

INSERT INTO sqlite_sequence (name, seq)
SELECT 'lab_presets_new', 0
 WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'lab_presets_new');

UPDATE sqlite_sequence
   SET seq = MAX(seq, COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'lab_presets'), 0))
 WHERE name = 'lab_presets_new';

DROP TABLE lab_presets;
ALTER TABLE lab_presets_new RENAME TO lab_presets;

COMMIT;

PRAGMA foreign_keys = ON;
