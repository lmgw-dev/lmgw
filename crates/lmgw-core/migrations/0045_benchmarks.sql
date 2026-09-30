-- Benchmark runs (benchmark design §7): one row per run of the suite against
-- one local chat row, written when the run starts, updated after every phase
-- (so a crash keeps the phases done so far) and finalised when it ends.
--
-- The JSON columns are the typed DTOs of `lmgw_api_types::bench` /
-- `bench_ops`, so the store, the ops and the Benchmarks page cannot disagree
-- about what a field means. Nothing prunes this table: runs are deleted by
-- hand only (a run is tens of KB).
--
-- `job_id` is not a foreign key, as on `build_runs`: job rows are pruned by
-- the jobs retention settings, a run's history must outlive that.
-- `model_id`, `gguf_path`, `gguf_size`, `quant`, `rung`, `image_ref`,
-- `image_id`, `settings_hash` and `suite_version` are columns (not only JSON)
-- because the runs table filters and the previous-comparable lookup (§6) ask
-- for them without parsing JSON. `gguf_mtime` is kept beside them so the
-- model identity (§6) reads back whole.
CREATE TABLE bench_runs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id        INTEGER,
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at   TEXT,
    status        TEXT NOT NULL DEFAULT 'running'
                  CHECK (status IN ('running','done','failed','canceled','aborted',
                                    'interrupted')),
    status_reason TEXT,
    error         TEXT,
    model_id      TEXT NOT NULL,
    gguf_path     TEXT NOT NULL DEFAULT '',
    gguf_size     INTEGER NOT NULL DEFAULT 0,
    gguf_mtime    TEXT,
    quant         TEXT,
    rung          INTEGER NOT NULL DEFAULT 0,
    image_ref     TEXT NOT NULL DEFAULT '',
    image_id      TEXT,
    build         TEXT NOT NULL DEFAULT '{}',   -- JSON BuildIdentity (§6 build)
    settings      TEXT NOT NULL DEFAULT '{}',   -- JSON BenchSettings (effective params + overrides)
    settings_hash TEXT NOT NULL DEFAULT '',
    command_line  TEXT NOT NULL DEFAULT '',
    gpu           TEXT NOT NULL DEFAULT '{}',   -- JSON GpuIdentity (§6 gpu)
    suite_version INTEGER NOT NULL DEFAULT 1,
    params        TEXT NOT NULL DEFAULT '{}',   -- JSON SuiteParams (repetitions, phases, G, sampling)
    results       TEXT NOT NULL DEFAULT '{}',   -- JSON RunResults (points per phase, energy, VRAM, load)
    probes        TEXT NOT NULL DEFAULT '{}',   -- JSON ProbeReport
    timeline      TEXT NOT NULL DEFAULT '{}',   -- JSON Timeline (samples at 500 ms, phase bands)
    notes         TEXT NOT NULL DEFAULT ''
);

CREATE INDEX bench_runs_model_idx ON bench_runs (model_id, id);
