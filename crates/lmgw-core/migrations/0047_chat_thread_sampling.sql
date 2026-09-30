-- Per-thread sampling parameters for the Chat page, sent with every turn on
-- top of the alias's own defaults (like `temperature` and `max_tokens`).
-- NULL = not set: the route's default applies. `stop` is a JSON array of
-- stop sequences, '[]' = none.
ALTER TABLE chat_threads ADD COLUMN top_p REAL;
ALTER TABLE chat_threads ADD COLUMN top_k INTEGER;
ALTER TABLE chat_threads ADD COLUMN min_p REAL;
ALTER TABLE chat_threads ADD COLUMN repeat_penalty REAL;
ALTER TABLE chat_threads ADD COLUMN presence_penalty REAL;
ALTER TABLE chat_threads ADD COLUMN frequency_penalty REAL;
ALTER TABLE chat_threads ADD COLUMN seed INTEGER;
ALTER TABLE chat_threads ADD COLUMN stop TEXT NOT NULL DEFAULT '[]';
