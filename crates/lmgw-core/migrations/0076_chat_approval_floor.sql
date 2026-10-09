-- The owner's approval floor (client-apps design §6.6): per thread and per
-- folder, the `require_approval` rules the owner last wrote, as a JSON list
-- of `mcp_tools` entries (`store::ThreadMcp`). A device's write of the
-- thread's or the folder's tools is compared with it as well as with the
-- stored rules, so removing a label and adding it back looser is refused.
-- Only the owner's writes set it; a device's never do.
ALTER TABLE chat_threads ADD COLUMN approval_floor TEXT NOT NULL DEFAULT '[]';
ALTER TABLE chat_folders ADD COLUMN approval_floor TEXT NOT NULL DEFAULT '[]';

-- The rules stored today were written before devices could only tighten
-- them: they are taken as the owner's. A row whose JSON does not parse
-- starts with no floor (the readers fall back the same way).
-- `CASE`, not `AND`: the JSON functions raise on text that is not JSON,
-- and only a `CASE` keeps them from running on it.
UPDATE chat_threads SET approval_floor = CASE
  WHEN NOT COALESCE(json_valid(mcp_tools), 0) THEN '[]'
  WHEN json_type(mcp_tools) = 'array' THEN mcp_tools
  ELSE '[]' END;
UPDATE chat_folders SET approval_floor = CASE
  WHEN NOT COALESCE(json_valid(defaults), 0) THEN '[]'
  WHEN json_type(defaults, '$.mcp_tools') = 'array' THEN json_extract(defaults, '$.mcp_tools')
  ELSE '[]' END;
