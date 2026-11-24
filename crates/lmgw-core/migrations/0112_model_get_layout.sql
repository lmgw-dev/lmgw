-- How Get it sets a model up where no single card holds its full context
-- (device-ledger design §13 WP12, the owner's decisions of 2026-11-17):
-- `split` over several cards at the full context, `one_card` at the largest
-- context one card holds, `experts_in_ram` with every layer's routed experts
-- in the computer's memory. NULL: the file fits one card, set up as before.
ALTER TABLE model_gets ADD COLUMN layout TEXT;
