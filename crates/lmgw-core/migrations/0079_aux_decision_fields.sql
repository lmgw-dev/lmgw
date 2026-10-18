-- Decision models (decision-models design §3.1, §3.3): a third aux kind, and
-- five fields every aux row gains — the flags an aux row could only carry in
-- its freeform `args` until now.
--
-- `kind` takes 'decision': a GGUF whose header carries `{arch}.decision.type`,
-- served by llama-server on `/v1/systemone` with no kind flag at all.
--
-- `ubatch_size`, `batch_size`: `--ubatch-size`, `--batch-size`. A non-causal
-- encoder evaluates its whole input in one ubatch, so the ubatch bounds every
-- input it can answer. NULL takes llama-server's default; a count is positive.
--
-- `mmproj_path`: `--mmproj`, relative to the aux models dir like `gguf_path`.
-- NULL is no projector; an empty path is not one either.
--
-- `backend`: NULL runs the row on the GPU, as every aux row did before; 'cpu'
-- runs it on the CPU — `--n-gpu-layers 0`, the class's run args without the
-- GPU passthrough, no VRAM charged, served under the GPU hold. No other value:
-- the GPU backend is the image's, not the row's.
--
-- `threads`: `--threads`. NULL renders nothing on the GPU and this machine's
-- physical cores on the CPU. A count is positive, with no upper cap.
--
-- Every existing row gets NULL in all five, so each renders the argv it
-- rendered before and boot adoption re-creates nothing. Flags already in a
-- row's `args` stay there; moving them into the fields is a separate step
-- (§3.3, WP2b).
--
-- SQLite cannot alter a CHECK, so the table is rebuilt. Nothing references
-- `aux_models` and it has no foreign keys, so this runs in sqlx's own
-- transaction, as 0070 does: a failure leaves the old table.
CREATE TABLE aux_models_new (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    model_id           TEXT NOT NULL UNIQUE,        -- client-facing id, `--alias`
    gguf_path          TEXT NOT NULL,               -- relative to the aux models dir
    kind               TEXT NOT NULL DEFAULT 'embed'
                       CHECK (kind IN ('embed','rerank','decision')),
    pooling            TEXT,                        -- --pooling (none|mean|cls|last|rank)
    ctx_size           INTEGER,                     -- --ctx-size (NULL = llama-server default)
    ubatch_size        INTEGER CHECK (ubatch_size IS NULL OR ubatch_size > 0),
    batch_size         INTEGER CHECK (batch_size IS NULL OR batch_size > 0),
    mmproj_path        TEXT CHECK (mmproj_path IS NULL OR mmproj_path <> ''),
    backend            TEXT CHECK (backend IS NULL OR backend = 'cpu'),
    threads            INTEGER CHECK (threads IS NULL OR threads > 0),
    args               TEXT NOT NULL DEFAULT '[]',  -- JSON [string] — freeform extra flags
    idle_seconds       INTEGER NOT NULL DEFAULT 0,  -- --sleep-idle-seconds (0 = never)
    enabled            INTEGER NOT NULL DEFAULT 1,
    image              TEXT,
    extra_run_args     TEXT,
    warm_start         INTEGER NOT NULL DEFAULT 0,
    hold_fallback_mode TEXT NOT NULL DEFAULT 'inherit',
    hold_fallback      TEXT,
    created_at         TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at         TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Every row with its id and timestamps.
INSERT INTO aux_models_new
    (id, model_id, gguf_path, kind, pooling, ctx_size, args, idle_seconds, enabled, image,
     extra_run_args, warm_start, hold_fallback_mode, hold_fallback, created_at, updated_at)
SELECT id, model_id, gguf_path, kind, pooling, ctx_size, args, idle_seconds, enabled, image,
       extra_run_args, warm_start, hold_fallback_mode, hold_fallback, created_at, updated_at
FROM aux_models;

-- Carry the AUTOINCREMENT high-water mark, as 0070 does for `prices`: `DROP
-- TABLE aux_models` takes its `sqlite_sequence` row with it, and the copy only
-- raises the new table's to max(id), so a deleted top id would be handed out
-- again. Two statements because the copy creates no sequence row for an empty
-- table.
INSERT INTO sqlite_sequence (name, seq)
SELECT 'aux_models_new', 0
 WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'aux_models_new');

UPDATE sqlite_sequence
   SET seq = MAX(seq, COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'aux_models'), 0))
 WHERE name = 'aux_models_new';

DROP TABLE aux_models;
ALTER TABLE aux_models_new RENAME TO aux_models;
