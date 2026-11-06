-- Stored `/v1/responses` are the key's that made them, and a response an
-- upstream made natively can be fetched again through lmgw (openai egress
-- design §6, *2026-10-24*).
--
-- `responses.key_id`: the `api_keys.id` of the request that made the
-- response. NULL is a request made without a key, and every row from before
-- this migration. An owner key reads every row; any other key only its own;
-- a request without a key only the NULL ones. No foreign key: `api_keys.id`
-- is AUTOINCREMENT, so a deleted key's id is never handed out again, and its
-- rows stay the owner's to read.
ALTER TABLE responses ADD COLUMN key_id INTEGER;

-- One row per response an upstream made natively (`responses/native.rs`):
-- its own id, from the whole body or from `response.created`. `GET`,
-- `DELETE /v1/responses/{id}` and `…/input_items` for such an id are
-- forwarded to that upstream, read by the same key rule as `responses`.
-- Gone with its upstream row: there is nothing to forward to then.
CREATE TABLE native_responses (
    id             TEXT PRIMARY KEY,               -- the upstream's response id
    upstream_id    INTEGER NOT NULL REFERENCES upstreams(id) ON DELETE CASCADE,
    key_id         INTEGER,                        -- as responses.key_id
    model          TEXT NOT NULL,                  -- the alias the client asked for
    upstream_model TEXT NOT NULL,                  -- the model the upstream was sent
    created_at     TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Age eviction, by the same retention setting as `responses`.
CREATE INDEX idx_native_responses_created ON native_responses(created_at);
