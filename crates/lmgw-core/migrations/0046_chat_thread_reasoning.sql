-- Per-thread reasoning overrides for the Chat page: the values a client would
-- send as `x-lmgw-reasoning` (on/off), `x-lmgw-reasoning-effort` and
-- `x-lmgw-reasoning-budget`, kept with the thread and sent at that same tier
-- with every turn — above the alias's own defaults, so a thread can try a
-- setting without the alias being edited. NULL = not overridden: the route's
-- default applies. `reasoning_enabled` is 0/1.
ALTER TABLE chat_threads ADD COLUMN reasoning_enabled INTEGER;
ALTER TABLE chat_threads ADD COLUMN reasoning_effort TEXT;
ALTER TABLE chat_threads ADD COLUMN reasoning_budget INTEGER;
