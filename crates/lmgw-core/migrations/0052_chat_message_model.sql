-- Which model answered a Chat reply (review R1 item c). The Markdown export
-- used to label every reply with the thread's *current* model, which a model
-- switch or a regenerate on another model makes wrong for every earlier one.
--
-- `model`: the alias the turn asked for — the thread's model when it ran.
-- `answered_by`: the alias that actually answered when it was not `model` —
-- a GPU-hold or outside-VRAM fallback, a ladder climb's fallback, a
-- candidate alias's pick. NULL when `model` itself answered.
-- Both NULL on user messages and on replies saved before this migration.
ALTER TABLE chat_messages ADD COLUMN model TEXT;
ALTER TABLE chat_messages ADD COLUMN answered_by TEXT;
