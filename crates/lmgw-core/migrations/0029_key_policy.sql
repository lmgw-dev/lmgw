-- Per-key scope, budget and rate limits (usage-analytics design §4).
--
-- An API key was a boolean: any key that works, works for every alias, forever,
-- at any rate. This is a single-owner desktop gateway, so "policy" here means
-- the owner fencing off their own keys from their own mistakes — an agent in a
-- loop, a script left running, an unattended corpus ingest against a cloud
-- embedder.
ALTER TABLE api_keys ADD COLUMN scope_mode TEXT NOT NULL DEFAULT 'all'
     CHECK (scope_mode IN ('all','allow','deny'));
-- Newline-delimited globs over alias names — the same syntax the dashboard's
-- textareas and the self-admin tools already use for lists.
ALTER TABLE api_keys ADD COLUMN scope_patterns TEXT NOT NULL DEFAULT '';
ALTER TABLE api_keys ADD COLUMN budget_micro INTEGER NOT NULL DEFAULT 0;  -- 0 = no budget
ALTER TABLE api_keys ADD COLUMN budget_period TEXT NOT NULL DEFAULT 'month'
     CHECK (budget_period IN ('day','month','total'));
ALTER TABLE api_keys ADD COLUMN rpm_limit INTEGER NOT NULL DEFAULT 0;         -- requests/min
ALTER TABLE api_keys ADD COLUMN tpm_limit INTEGER NOT NULL DEFAULT 0;         -- tokens/min
ALTER TABLE api_keys ADD COLUMN concurrency_limit INTEGER NOT NULL DEFAULT 0;
ALTER TABLE api_keys ADD COLUMN expires_at TEXT;
ALTER TABLE api_keys ADD COLUMN note TEXT NOT NULL DEFAULT '';

-- Internal consumers are identities too (§4.4). Admin Chat, quickdoc ingest,
-- golden-query generation, the mail workflow and the /v1/responses tool loop
-- all spend real money on cloud aliases and have always logged as "no key".
-- They get rows here so they are visible and budgetable — never authenticable:
-- `kind='internal'` rows carry no usable hash and the auth path must skip them.
ALTER TABLE api_keys ADD COLUMN kind TEXT NOT NULL DEFAULT 'key'
     CHECK (kind IN ('key','internal'));

INSERT INTO api_keys (name, key_hash, enabled, kind, note) VALUES
  ('internal:admin-chat',      '', 1, 'internal', 'Admin Chat tool loop'),
  ('internal:chat',            '', 1, 'internal', 'Dashboard Chat page'),
  ('internal:responses',       '', 1, 'internal', '/v1/responses server-side tool loop'),
  ('internal:quickdoc-ingest', '', 1, 'internal', 'Corpus ingest: extraction + embedding'),
  ('internal:quickdoc-query',  '', 1, 'internal', 'docs__* retrieval: embed + rerank'),
  ('internal:quickdoc-eval',   '', 1, 'internal', 'Golden-query generation and eval runs'),
  ('internal:workflow-mail',   '', 1, 'internal', 'Mail cleanup classification'),
  ('internal:mcp-sampling',    '', 1, 'internal', 'MCP sampling requests from southbound servers');
