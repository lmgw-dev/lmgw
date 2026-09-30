-- Per-model container overrides (per-model-containers design §3.1, §6).
--
-- `image`/`extra_run_args` default to NULL, meaning "inherit the owning
-- class's `RouterSettings`/`AudioSettings`" — a model row that has never
-- touched these keeps behaving exactly as it does today. `warm_start`
-- defaults to 0 (off): nothing auto-starts on its own yet, this only adds
-- the column the per-model runtime's boot-time pass will read.
--
-- Purely additive: no existing column changes shape, and nothing reads these
-- three yet outside the new runtime descriptor (§3.1) this migration's work
-- package introduces alongside it.
ALTER TABLE local_models ADD COLUMN image TEXT;
ALTER TABLE local_models ADD COLUMN extra_run_args TEXT;
ALTER TABLE local_models ADD COLUMN warm_start INTEGER NOT NULL DEFAULT 0;

ALTER TABLE aux_models ADD COLUMN image TEXT;
ALTER TABLE aux_models ADD COLUMN extra_run_args TEXT;
ALTER TABLE aux_models ADD COLUMN warm_start INTEGER NOT NULL DEFAULT 0;

ALTER TABLE audio_models ADD COLUMN image TEXT;
ALTER TABLE audio_models ADD COLUMN extra_run_args TEXT;
ALTER TABLE audio_models ADD COLUMN warm_start INTEGER NOT NULL DEFAULT 0;
