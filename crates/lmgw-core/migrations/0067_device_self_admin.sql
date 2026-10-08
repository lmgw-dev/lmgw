-- A device may use lmgw's admin tools (client-apps design L3/L5, the
-- owner's decision of 2026-10-07): one switch per device key, off by
-- default, set by the owner only (key_create, key_set). With it on, the
-- device sees and uses the Chat threads and folders that carry the
-- self-admin toolset (`lmgw`), its tool scope includes the label, and it
-- may attach it itself; the global self-admin level still bounds what the
-- tools do. Admin Chat stays out of every device's reach.
--
-- Only a device row carries it, as the CHECK on `hosts_label` says for the
-- hosting grant.
ALTER TABLE api_keys ADD COLUMN self_admin INTEGER NOT NULL DEFAULT 0
    CHECK (self_admin = 0 OR kind = 'device');

-- The change feed's `admin` column becomes a level (`self_admin_thread!`):
-- 0 every reader sees it, 1 a device sees it only with the switch on (the
-- self-admin toolset), 2 no device sees it (Admin Chat, a folder a device
-- deleted). Every record written before was "no device sees it" when it
-- was not 0, and no device could have the switch then: they all become 2,
-- which a device that is given the switch later never reads, and its
-- stream hears the toolset's threads and folders as created when the
-- switch goes on. The `admin_was` a write that flipped the flag stored
-- reads the same way.
UPDATE chat_feed SET admin = 2 WHERE admin != 0;
UPDATE chat_feed
   SET detail = json_set(detail, '$.admin_was', 2)
 WHERE json_valid(detail) AND json_extract(detail, '$.admin_was') = 1;
