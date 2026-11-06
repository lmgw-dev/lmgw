-- The output items of each native response, under the key that made it
-- (openai egress design §6.1, WP5b).
--
-- OpenAI resolves an input item's `id` (a `reasoning` item's `rs_…`, a
-- `message`'s `msg_…`, a `*_call`'s) and an `item_reference` against what it
-- stores under the provider key, which is the owner's. A client key may name
-- only the items of answers it was given itself (`responses/references.rs`),
-- so every native answer's item ids are recorded here, as the meter reads
-- them.
--
-- `native_responses.stored`: whether the upstream keeps the response
-- (`store` not false). A `store: false` answer is recorded too, so its items
-- are its key's, but it is never fetched again: the upstream has nothing to
-- return, and `GET`, `DELETE`, `input_items` and `previous_response_id`
-- read only the stored rows. 1 for every row from before this migration,
-- which recorded only stored responses.
ALTER TABLE native_responses ADD COLUMN stored INTEGER NOT NULL DEFAULT 1;

-- One row per output item id. Gone with its response record, so the
-- response's retention and its upstream row's deletion take the items along.
CREATE TABLE native_response_items (
    item_id     TEXT PRIMARY KEY,                 -- the upstream's item id
    response_id TEXT NOT NULL REFERENCES native_responses(id) ON DELETE CASCADE,
    key_id      INTEGER,                          -- as native_responses.key_id
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- The cascade from `native_responses` finds a response's items by it.
CREATE INDEX idx_native_response_items_response ON native_response_items(response_id);
