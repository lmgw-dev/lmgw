-- The fourth local model class: image generation through stable-diffusion.cpp's
-- `sd-server`, one container per model (image-generation design §3, §4).
--
-- Shaped like `audio_models` (own table, own dir, own prefix) with two
-- differences the engine forces:
--
--   * `files` and `args` are JSON **objects**, not columns. One row is one
--     pipeline, and which weight files a pipeline needs depends on its family
--     (`--diffusion-model` + `--vae` + `--llm` for Z-Image, `-m` alone for an
--     SDXL checkpoint, plus `--clip_l`/`--clip_g`/`--t5xxl` for FLUX). sd.cpp
--     adds a family — and its flag — every few weeks, so the keys are
--     validated against the image's own `--help` (`sdcpp_caps`) rather than
--     frozen into nineteen columns here. Keys are the long flag with `-` → `_`.
--   * `idle_seconds` is real from day one, defaulting to the same 300 seconds
--     `local_models` has carried since 0001. Audio's missing idle column is a
--     known gap; this class does not inherit it, because a loaded FLUX
--     pipeline is 12+ GiB of VRAM nothing else can use (§3).
--
-- `modes` is what the operator says this pipeline does (`["img_gen"]`,
-- `["img_gen","vid_gen"]`) — checked against the probed `supported_modes` at
-- start, never silently corrected — and `edit` is a hard gate rather than a
-- hint: `/v1/images/edits` against a non-edit pipeline segfaults sd-server
-- (measured, §12.8).
--
-- The remaining columns are byte-for-byte the ones `local_models`/`aux_models`/
-- `audio_models` gained in 0022 (image, extra_run_args, warm_start), 0024
-- (hold_fallback_mode, hold_fallback) and 0025 (capabilities_override).
CREATE TABLE image_models (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    model_id              TEXT NOT NULL UNIQUE,      -- client-facing id under the image prefix
    files                 TEXT NOT NULL DEFAULT '{}',   -- JSON object — flag key → path relative to the image models dir
    args                  TEXT NOT NULL DEFAULT '{}',   -- JSON object — runtime + default-generation flags
    modes                 TEXT NOT NULL DEFAULT '["img_gen"]',  -- JSON array — img_gen | vid_gen
    edit                  INTEGER NOT NULL DEFAULT 0,   -- the pipeline takes reference images (/v1/images/edits)
    enabled               INTEGER NOT NULL DEFAULT 1,
    image                 TEXT,                         -- NULL inherits settings.image.image
    extra_run_args        TEXT,                         -- JSON [string]; NULL inherits the class default
    warm_start            INTEGER NOT NULL DEFAULT 0,
    idle_seconds          INTEGER NOT NULL DEFAULT 300, -- as local_models since 0001 (0 = never reap)
    hold_fallback_mode    TEXT NOT NULL DEFAULT 'inherit',
    hold_fallback         TEXT,
    capabilities_override TEXT,
    created_at            TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at            TEXT NOT NULL DEFAULT (datetime('now'))
);
