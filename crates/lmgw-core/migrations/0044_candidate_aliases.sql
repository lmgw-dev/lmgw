-- Candidate aliases (candidate-aliases design §4.1): an alias whose target is
-- a **primary** local chat model plus an ordered list of **alternates**, a
-- background flag, one whole-alias fallback and an explicit capability
-- contract. Column conventions mirror `models` (the plain-alias table, id/
-- alias/enabled/timestamps) and `local_models` (`fallback_mode`/`fallback`
-- is the same two-column split as `hold_fallback_mode`/`hold_fallback`, for
-- the reason `HoldFallbackMode`'s own doc comment gives: a lone
-- `Option<String>` cannot tell "inherit" from "explicitly none" from "not
-- touched by this patch" at once) and `builds` (`notes`, a free-text column
-- with no meaning to lmgw itself).
--
-- `candidates` is a JSON array of local chat model ids (`local_models.
-- model_id` — a row's own id, not a client-facing/prefixed name): the first
-- entry is the primary, the rest are alternates in preference order (§4.1).
-- `capabilities_disabled` is the owner's explicit "turn this common facet
-- off" list; `capabilities_enabled` is what every save actually computes and
-- stores (§12 entry 49) — the common set minus disabled, recomputed at every
-- save so a facet that becomes common again (the odd candidate removed) can
-- come back on, but never recomputed merely by reading the row, so a
-- candidate edited afterwards to drop a facet cannot silently shrink the
-- published contract — it is reported as a problem and skipped instead.
CREATE TABLE candidate_aliases (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    alias                 TEXT NOT NULL UNIQUE,
    candidates            TEXT NOT NULL DEFAULT '[]',  -- JSON [model_id, ...]
    background            INTEGER NOT NULL DEFAULT 0,
    fallback_mode         TEXT NOT NULL DEFAULT 'inherit'
                          CHECK (fallback_mode IN ('inherit','none','alias')),
    fallback              TEXT,
    capabilities_disabled TEXT NOT NULL DEFAULT '[]',  -- JSON [facet, ...]
    capabilities_enabled  TEXT NOT NULL DEFAULT '[]',  -- JSON [facet, ...]
    enabled               INTEGER NOT NULL DEFAULT 1,
    notes                 TEXT NOT NULL DEFAULT '',
    created_at            TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at            TEXT NOT NULL DEFAULT (datetime('now'))
);
