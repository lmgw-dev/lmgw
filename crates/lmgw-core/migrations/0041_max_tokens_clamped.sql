-- The request gate's clamp column (ladder design §3.2, unified-KV design
-- §3.3 step 1; shared-gate spec §6): a ladder rung or a guarded unified-KV
-- pool requires `n_predict` as its ceiling, and lmgw lowers a client's
-- `max_tokens` to it when the client asked for more. `max_tokens_clamped`
-- records the value it was lowered *to*, the same request that carries the
-- `x-lmgw-max-tokens-clamped` response header. NULL = not clamped — either
-- the row is unguarded, or the client's value already fit under the ceiling.
ALTER TABLE request_logs ADD COLUMN max_tokens_clamped INTEGER;
