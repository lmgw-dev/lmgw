-- Get it, server-side (find-a-model design §6.2, D25, D26, D122).
--
-- `model_gets` holds one Get it's intent: the file, the class, the measure
-- the card showed, the model id it sets up, the download rows it waits for,
-- and how it ended. A `model_get` job runs it (wait, plan, fit, create), and
-- the boot gives every row still `running` its job again, after the cut
-- downloads went on: a restart mid-get goes on, and the create step answers
-- a row that already serves the file instead of making a second one.
--
-- One live get per file: a second Get it of the same file joins the first.
CREATE TABLE model_gets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL,
    file TEXT NOT NULL,
    target TEXT NOT NULL CHECK (target IN ('chat', 'aux')),
    measure TEXT NOT NULL CHECK (measure IN ('whole_card', 'beside')),
    model_id TEXT NOT NULL,
    -- An aux get's kind (`embed`, `rerank`, `decision`), read from the
    -- header before the download; NULL for chat.
    kind TEXT CHECK (kind IS NULL OR kind IN ('embed', 'rerank', 'decision')),
    -- JSON array of `hf_models` ids: every file of the get. Empty until the
    -- op has queued them: the row is written first, so a request dropped
    -- mid-way never leaves downloads without their get.
    download_ids TEXT NOT NULL DEFAULT '[]',
    -- The ids of `download_ids` this get started itself. The others it
    -- joined (a projector another get, or `hf_add`, was already fetching):
    -- the get's cancel stops only its own.
    queued_ids TEXT NOT NULL DEFAULT '[]',
    -- The fit note the row it creates takes (the JSON of `FitNote`),
    -- recorded before the create: a get cut between the create and the
    -- note's write writes it when it re-attaches.
    note TEXT,
    -- The job running the get now (the boot gives it a new one).
    job_id INTEGER,
    status TEXT NOT NULL DEFAULT 'running'
        CHECK (status IN ('running', 'done', 'failed', 'canceled')),
    error TEXT,
    -- The row it set up (or found serving the file), in its class's table.
    row_id INTEGER,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at TEXT
);
CREATE UNIQUE INDEX model_gets_one_live ON model_gets (repo, file) WHERE status = 'running';

-- The note a row keeps when its context is a fit's shorter one (D26): the
-- context, the trained one, the measure and its bytes, and when, as the
-- JSON of `FitNote`. Get it writes it, the model editor's "Fit again"
-- renews it. A note only stands while the context it records does: it goes
-- with its row, and with a change of the row's context by any other save.
CREATE TABLE model_fit_notes (
    class TEXT NOT NULL CHECK (class IN ('chat', 'aux')),
    row_id INTEGER NOT NULL,
    note TEXT NOT NULL,
    PRIMARY KEY (class, row_id)
);

CREATE TRIGGER local_models_fit_note_gone AFTER DELETE ON local_models
BEGIN
    DELETE FROM model_fit_notes WHERE class = 'chat' AND row_id = OLD.id;
END;

CREATE TRIGGER aux_models_fit_note_gone AFTER DELETE ON aux_models
BEGIN
    DELETE FROM model_fit_notes WHERE class = 'aux' AND row_id = OLD.id;
END;

CREATE TRIGGER local_models_fit_note_ctx AFTER UPDATE OF params ON local_models
WHEN json_extract(OLD.params, '$.ctx_size') IS NOT json_extract(NEW.params, '$.ctx_size')
BEGIN
    DELETE FROM model_fit_notes WHERE class = 'chat' AND row_id = NEW.id;
END;

CREATE TRIGGER aux_models_fit_note_ctx AFTER UPDATE OF ctx_size ON aux_models
WHEN OLD.ctx_size IS NOT NEW.ctx_size
BEGIN
    DELETE FROM model_fit_notes WHERE class = 'aux' AND row_id = NEW.id;
END;
