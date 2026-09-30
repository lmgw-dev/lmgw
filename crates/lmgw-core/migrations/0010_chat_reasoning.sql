-- Reasoning models (e.g. LFM2.5, DeepSeek-R1) stream their "thinking" in a
-- separate `reasoning_content` channel; persist it alongside the answer so a
-- reasoning-only turn (model that spent its budget thinking) isn't blank on
-- reload, and the thought trace survives a page refresh.
ALTER TABLE chat_messages ADD COLUMN reasoning TEXT NOT NULL DEFAULT '';
