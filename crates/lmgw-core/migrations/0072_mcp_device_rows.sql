-- Device-hosted MCP (client-apps design §5.2): a paired device's hosted tools
-- are an `mcp_servers` row of transport 'device', owned by the device key the
-- way an agent's own row is owned by its agent (`agent_id`, 0032).
--
-- The hosting grant (`api_keys.hosts_label`, 0061) creates the row, keeps it in
-- step (prefix = the label, name = the device key's name) and deletes it with
-- the grant or the key. The URL and command columns stay unused on it;
-- `idle_seconds` 0 (never reaped), `autostart` 0 (lmgw never dials it: the
-- device does), `allow_sampling` 0 (refused on a device row, R26).
--
-- A soft link, as every `key_id` column is (0061's note): the key ops delete
-- the row in the same transaction as the key or the grant.
ALTER TABLE mcp_servers ADD COLUMN device_key_id INTEGER;

-- One row per device.
CREATE UNIQUE INDEX mcp_servers_device ON mcp_servers(device_key_id)
    WHERE device_key_id IS NOT NULL;

-- Every grant made before this file gets its row, unless a server already has
-- the device key's name (`device:<name>`; from this release on, a server name
-- with that prefix is refused for every writer but the grant, as `agent:` is).
INSERT INTO mcp_servers
       (name, enabled, transport, tool_prefix, timeout_ms, autostart, idle_seconds,
        allow_sampling, device_key_id)
SELECT k.name, 1, 'device', k.hosts_label, 60000, 0, 0, 0, k.id
  FROM api_keys k
 WHERE k.kind = 'device'
   AND k.hosts_label IS NOT NULL
   AND NOT EXISTS (SELECT 1 FROM mcp_servers s WHERE s.name = k.name);
