-- A device's admin tools become a level (client-apps design L3/L5, the
-- owner's decision of 2026-10-07 on the pre-merge review's P-3):
-- `api_keys.self_admin` is 0 off, 1 read only, 2 full. What the tools may
-- do is the device's level capped by the gateway's own self-admin level, and
-- the tools that register programs this machine runs as the lmgw user (an
-- MCP server's command, containers, agents, builds) are write tools, so only
-- a device at full under a gateway at full reaches them.
--
-- Every row with the switch on holds 1 already, which now reads as read
-- only: the safer level for the transition, and nothing has shipped. The
-- column keeps its CHECK (only a device row carries a level); these
-- triggers hold its values to the three levels, as a CHECK would — SQLite
-- cannot add one to an existing column without rebuilding the table.
CREATE TRIGGER api_keys_self_admin_level_insert
BEFORE INSERT ON api_keys
WHEN NEW.self_admin NOT IN (0, 1, 2)
BEGIN
    SELECT RAISE(ABORT, 'api_keys.self_admin is a level: 0 off, 1 read only, 2 full');
END;

CREATE TRIGGER api_keys_self_admin_level_update
BEFORE UPDATE OF self_admin ON api_keys
WHEN NEW.self_admin NOT IN (0, 1, 2)
BEGIN
    SELECT RAISE(ABORT, 'api_keys.self_admin is a level: 0 off, 1 read only, 2 full');
END;
