-- §21: per-upstream native /v1/responses support.
--
-- Off by default, including for existing rows: "OpenAI-compatible" does not
-- imply "Responses-capable" (llama-server, vLLM and most local servers expose
-- only /v1/chat/completions), and guessing wrong turns every request into a
-- 404. When it is on, the gateway forwards the request body verbatim instead
-- of synthesizing the API from chat/completions — which is what preserves
-- reasoning continuity on upstreams that really do implement it.
ALTER TABLE upstreams ADD COLUMN supports_responses INTEGER NOT NULL DEFAULT 0;
