-- Which paired device created an agent (client-apps design L5's note,
-- 2026-10-07): an agent a device installs through lmgw__agent_set or
-- lmgw__agent_install is recorded as its own, and a device may replace only
-- an agent it created, so its write cannot turn the owner's agent into one
-- that runs what the device chose. NULL for the owner's agents, the
-- built-ins and every row from before. No foreign key: a deleted device's
-- agents stay, and the id still says who made them.
ALTER TABLE agents ADD COLUMN created_by_key INTEGER;
