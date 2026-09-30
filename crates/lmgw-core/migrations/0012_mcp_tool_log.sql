-- MCP tool-call logging (§10, milestone 3). Northbound `tools/call`s land in the
-- same unified `request_logs` feed as LLM traffic, with `ingress_proto = 'mcp'`
-- and the registered server's name in `upstream_name`. The *tool* name gets its
-- own nullable column rather than overloading `upstream_model`, so the dashboard
-- can filter/display "which tool" cleanly (decided in §10/§20). NULL for every
-- non-MCP row (LLM requests, workflow/chat, mcp-sampling).
ALTER TABLE request_logs ADD COLUMN mcp_tool TEXT;
