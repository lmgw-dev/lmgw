-- The device-tools grant (client-apps design L16 as changed 2026-10-15): a
-- key's policy names the device-hosted labels whose tools it reaches, as a
-- JSON list (`["kai"]`), whatever its tool scope's mode. The tool scope then
-- filters which of those tools it sees, as it does any server's. Before this,
-- only an `allow` pattern naming the label (`kai__*`) reached it, so an `all`
-- key could not have every tool plus Kai's.
ALTER TABLE api_keys ADD COLUMN device_tools TEXT NOT NULL DEFAULT '[]';

-- The grant is now the only way a key reaches a hosted label, and its tool
-- scope filters the label's tools like any server's. A client key or device
-- whose `allow` patterns named a label (a pattern whose text before its first
-- `*` starts with `<label>__`, case-insensitively) gets it granted here, so it
-- reaches what it reached, but only when no other line could match the
-- label's tools: a line with a `*`, not naming the label, whose text before
-- that `*` is a prefix of `<label>__` (`*`, `k*`, `ka*`, `*__search`). Such a
-- mixed list (`*` and `kai__notify`) used to reach only the naming line's
-- tools; granted, its broad line would admit every tool of the label. It
-- gets no grant instead, losing reach rather than gaining it; the boot log
-- says which keys and the Keys page flags the pattern. The same rule is
-- `lmgw_api_types::scope::migration_grants`. `substr`, not `LIKE`: `_` is a
-- LIKE wildcard. The hosting device's own label is not granted: it reaches it
-- anyway.
WITH RECURSIVE lines(id, rest, line) AS (
    SELECT id, tool_scope_patterns || char(10), NULL
      FROM api_keys
     WHERE tool_scope_mode = 'allow' AND kind IN ('key', 'device')
    UNION ALL
    SELECT id,
           substr(rest, instr(rest, char(10)) + 1),
           trim(substr(rest, 1, instr(rest, char(10)) - 1))
      FROM lines
     WHERE rest <> ''
),
named(id, label) AS (
    SELECT DISTINCT l.id, h.hosts_label
      FROM lines l
      JOIN api_keys h
        ON h.kind = 'device' AND h.hosts_label IS NOT NULL AND h.hosts_label <> ''
       AND h.id <> l.id
     WHERE l.line IS NOT NULL AND l.line <> ''
       AND substr(lower(substr(l.line, 1, instr(l.line || '*', '*') - 1)), 1,
                  length(h.hosts_label) + 2) = lower(h.hosts_label) || '__'
       AND NOT EXISTS (
            SELECT 1
              FROM lines b
             WHERE b.id = l.id AND b.line IS NOT NULL AND b.line <> ''
               AND instr(b.line, '*') > 0
               AND substr(lower(substr(b.line, 1, instr(b.line, '*') - 1)), 1,
                          length(h.hosts_label) + 2) <> lower(h.hosts_label) || '__'
               AND lower(substr(b.line, 1, instr(b.line, '*') - 1))
                   = substr(lower(h.hosts_label) || '__', 1, instr(b.line, '*') - 1)
       )
)
UPDATE api_keys
   SET device_tools = (SELECT json_group_array(label)
                         FROM (SELECT label FROM named n
                                WHERE n.id = api_keys.id ORDER BY label))
 WHERE id IN (SELECT id FROM named);
