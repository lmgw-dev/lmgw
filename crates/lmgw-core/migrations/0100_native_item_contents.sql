-- Every encrypted content a recorded output item was answered with
-- (api-compat design §9, finding 9; WP8 review S-R3).
--
-- 0096 kept one hash per item id, the first one recorded. OpenAI encrypts a
-- reasoning item anew each time it serialises it (Fernet, a random IV), so a
-- stream's `response.output_item.done` and its `response.completed` output
-- carry different `encrypted_content` for the same `rs_…` id. lmgw recorded
-- the item's `done`, a client that replays the final response's output sends
-- the other, and the check answered 404. Each variant the answer carried is
-- now a row here, and a returned item passes with any of them.
CREATE TABLE native_response_item_contents (
    item_id          TEXT NOT NULL REFERENCES native_response_items(item_id) ON DELETE CASCADE,
    encrypted_sha256 TEXT NOT NULL,               -- as ir::encrypted_sha256
    PRIMARY KEY (item_id, encrypted_sha256)
);

INSERT INTO native_response_item_contents (item_id, encrypted_sha256)
    SELECT item_id, encrypted_sha256 FROM native_response_items
    WHERE encrypted_sha256 IS NOT NULL;

ALTER TABLE native_response_items DROP COLUMN encrypted_sha256;
