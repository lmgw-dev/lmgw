-- Eval history and the regression badge (§10, §11).
--
-- `corpus.eval_score` already held the latest measured hit rate. Regression is
-- a *comparison*, so it needs the thing to compare against: `eval_best` is the
-- highest score this corpus has ever measured, and the badge is simply "the
-- latest run is below it". Storing the flag rather than deriving it on read
-- keeps `docs__resolve` a single query, and keeps the UI and the tool reporting
-- the same bit rather than two re-derivations of it.

ALTER TABLE corpus ADD COLUMN eval_best REAL;
ALTER TABLE corpus ADD COLUMN eval_regression INTEGER NOT NULL DEFAULT 0;
ALTER TABLE corpus ADD COLUMN eval_at TEXT NOT NULL DEFAULT '';
-- Depth the stored score was measured at: hit@1 and hit@10 are different
-- numbers, and a score with no k next to it is not a score.
ALTER TABLE corpus ADD COLUMN eval_k INTEGER NOT NULL DEFAULT 0;

-- One `eval_run` job's outcome, retained so a score can be read as a trend
-- rather than a single number. `report` is the full per-query breakdown the
-- eval view renders; `params` are the §6 stage settings it was measured under,
-- without which two runs are not comparable.
CREATE TABLE eval_run (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    corpus_id        INTEGER NOT NULL REFERENCES corpus(id) ON DELETE CASCADE,
    k                INTEGER NOT NULL,
    queries          INTEGER NOT NULL DEFAULT 0,
    hit_at_k         REAL NOT NULL DEFAULT 0,
    mrr              REAL NOT NULL DEFAULT 0,
    orphaned_queries INTEGER NOT NULL DEFAULT 0,
    -- Whether *this* run was the one that lit the badge.
    regression       INTEGER NOT NULL DEFAULT 0,
    params           TEXT NOT NULL DEFAULT '{}',
    report           TEXT NOT NULL DEFAULT '{}',
    created_at       TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX eval_run_corpus ON eval_run(corpus_id, id DESC);
