-- Question sets and set runs (labs-store design §3.1), slice 2 of the lab
-- store. A preset is a named form part a lab keeps: the Decision lab's
-- question sets now, an image prompt and audio settings later. A batch is a
-- set run: the set as it was run (later edits of the preset do not rewrite
-- results), the model, the file's name and the column mapping, and the job
-- that last ran it. The job id has no foreign key, because finished job rows
-- are pruned and the batch outlives them.
--
-- A set run's items are lab groups with `batch_id` set, one cell each: their
-- forms hold the set with the item's state, `expected` the item's labels
-- ({question id: label}, NULL when it has none), `item_key` the value of the
-- mapping's key column (NULL without one). The history lists only the
-- groups with no batch; usage and export count both.
--
-- A set run is written in several short write transactions, so a file of a
-- few hundred thousand rows does not hold the write lock past the busy
-- timeout: the run first with `ready` 0, its items in chunks, and `ready` 1
-- with the last chunk. A run that is not ready is never run, listed or
-- counted, nor are its items; a boot deletes what a crash left of one.
--
-- `lab_runs.readable` judges a set run item's 2xx run when it is recorded:
-- 1 when it reads as an answer to every question of the item's form, 0
-- when it does not; NULL for a failed run and for any run not of a set run,
-- which nothing judges.
CREATE TABLE lab_presets (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    lab         TEXT NOT NULL CHECK (lab IN ('decision', 'image', 'audio')),
    kind        TEXT NOT NULL,
    name        TEXT NOT NULL,
    body        TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (lab, kind, name)
);

CREATE TABLE lab_batches (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    lab             TEXT NOT NULL CHECK (lab IN ('decision', 'image', 'audio')),
    preset_id       INTEGER REFERENCES lab_presets(id) ON DELETE SET NULL,
    preset_snapshot TEXT NOT NULL,
    model           TEXT NOT NULL,
    source_name     TEXT NOT NULL,
    mapping         TEXT NOT NULL,
    item_count      INTEGER NOT NULL,
    concurrency     INTEGER NOT NULL CHECK (concurrency > 0),
    job_id          INTEGER,
    ready           INTEGER NOT NULL DEFAULT 0 CHECK (ready IN (0, 1)),
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX idx_lab_batches_lab ON lab_batches (lab, id DESC);
CREATE INDEX idx_lab_batches_preset ON lab_batches (preset_id);

ALTER TABLE lab_groups ADD COLUMN batch_id INTEGER REFERENCES lab_batches(id) ON DELETE CASCADE;
ALTER TABLE lab_groups ADD COLUMN item_ix INTEGER;
ALTER TABLE lab_groups ADD COLUMN expected TEXT;
ALTER TABLE lab_groups ADD COLUMN item_key TEXT;
CREATE INDEX idx_lab_groups_batch ON lab_groups (batch_id, item_ix);

ALTER TABLE lab_runs ADD COLUMN readable INTEGER CHECK (readable IN (0, 1));
