-- Container builds from git (container-builds design §3–§5): the Backends page.
--
-- `builds` is the editable definition of what to build: a repository, a ref,
-- an ordered list of extras merged on top, the GPU backend and the build
-- knobs. `build_runs` is one execution of it — append-only, and kept when its
-- build is deleted (`build_id` goes NULL; the slug and engine are snapshotted
-- on the run so it stays readable). There is deliberately no image table:
-- podman is the source of truth for images, and each image lmgw builds
-- carries its provenance in `dev.lmgw.*` labels (§3 "Image").
--
-- NULL on the optional `builds` columns means **auto**, resolved per run from
-- the engine preset and the host and recorded in that run's `inputs`:
-- `cuda_version` → the preset default, `arch` → the host GPUs' compute
-- capabilities, `dockerfile`/`target` → the preset's candidates, and `edits`
-- → the preset's edits for the resolved Dockerfile. A non-NULL `edits` is the
-- owner's explicit list (which may be `[]`: no edits at all). `keep_runs` NULL
-- keeps every run's immutable tag.
--
-- `slug` becomes the tag (`localhost/lmgw-<engine>:<slug>`). Its charset, its
-- length bound and its immutability once a run exists are enforced where it is
-- written (`backends::validate`, `store::update_build`), not here: a CHECK
-- could say none of those three things in a sentence an owner can act on.
CREATE TABLE builds (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    slug            TEXT NOT NULL UNIQUE,
    name            TEXT NOT NULL,
    engine          TEXT NOT NULL CHECK (engine IN ('llama','audio','sdcpp')),
    repo_url        TEXT NOT NULL,
    forge           TEXT NOT NULL DEFAULT 'plain' CHECK (forge IN ('github','gitlab','plain')),
    ref             TEXT NOT NULL,
    extras          TEXT NOT NULL DEFAULT '[]',   -- JSON [{kind: pr|ref, …}], in merge order
    backend         TEXT NOT NULL DEFAULT 'cuda' CHECK (backend IN ('cuda','vulkan','rocm','cpu')),
    cuda_version    TEXT,                         -- NULL = the preset default
    arch            TEXT,                         -- JSON [string]; NULL = auto-detect
    dockerfile      TEXT,                         -- NULL = auto
    target          TEXT,                         -- NULL = auto
    edits           TEXT,                         -- JSON [{find, replace, required}]; NULL = preset
    ccache          INTEGER NOT NULL DEFAULT 1,
    ccache_max_size TEXT NOT NULL DEFAULT '10G',
    cpus            TEXT,                         -- --cpuset-cpus; NULL = all cores
    build_args      TEXT NOT NULL DEFAULT '',     -- KEY=VALUE lines
    keep_layers     INTEGER NOT NULL DEFAULT 0,
    keep_runs       INTEGER CHECK (keep_runs IS NULL OR keep_runs >= 0),  -- NULL = keep all
    notes           TEXT NOT NULL DEFAULT '',
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

-- `job_id` is not a foreign key: job rows are pruned by the jobs retention
-- settings, and a run's history must outlive that. `inputs` is the full
-- snapshot (the build definition as built, what every "auto" and every
-- followed head resolved to, the config hash); `base_sha` and `cfg_hash` are
-- also columns because update detection and the "already built" check ask for
-- them without parsing JSON. `promoted` marks the run whose image lmgw last
-- moved the build's moving tag to — at most one per build, maintained by
-- `store::promote_build_run`.
CREATE TABLE build_runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    build_id    INTEGER REFERENCES builds(id) ON DELETE SET NULL,
    job_id      INTEGER,
    slug        TEXT NOT NULL,
    engine      TEXT NOT NULL CHECK (engine IN ('llama','audio','sdcpp')),
    trigger     TEXT NOT NULL DEFAULT 'manual' CHECK (trigger IN ('manual','mcp','schedule')),
    status      TEXT NOT NULL DEFAULT 'running'
                CHECK (status IN ('running','succeeded','unverified','broken','failed',
                                  'canceled','up_to_date')),
    inputs      TEXT NOT NULL DEFAULT '{}',
    base_sha    TEXT,
    cfg_hash    TEXT,
    image_id    TEXT,
    tags        TEXT NOT NULL DEFAULT '[]',       -- JSON [string]
    promoted    INTEGER NOT NULL DEFAULT 0,
    size_bytes  INTEGER,
    verify      TEXT,                             -- JSON, shaped by the executor
    error       TEXT,
    log_path    TEXT,
    started_at  TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at TEXT
);

CREATE INDEX idx_build_runs_build ON build_runs(build_id, started_at);
