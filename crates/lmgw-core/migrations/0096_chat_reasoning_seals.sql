-- A plain Chat reply's reasoning seals (openai egress design §7.3.2, WP8),
-- and the hash of each recorded item's encrypted reasoning (below).
--
-- The OpenAI platform answers lmgw's own sends with `store: false`, so it
-- keeps nothing between turns: a reasoning item's `encrypted_content` is
-- what lets the next turn give the model its earlier reasoning back. A tool
-- turn keeps its IR whole in `ir_messages`, its seals inside it; a plain
-- turn keeps its trace in `reasoning`, which is text. This column is that
-- turn's seals: a JSON array of `store::ReplySeal` objects (`at`, the byte
-- offset into `content` where the item was answered, and the `ir::Seal`'s
-- `by`, `upstream_id`, `config`, `item_id`, `data`), in the order the items
-- were answered, written beside `reasoning` and cleared with it. NULL for a
-- reply without a seal, and for every row from before this migration.
ALTER TABLE chat_messages ADD COLUMN reasoning_seals TEXT;

-- A recorded output item's `encrypted_content`, hashed (WP8 review S-R3): the
-- SHA-256 (hex) of a reasoning item's encrypted reasoning, written by every
-- path that records items (the platform's own sends, the `/v1/responses`
-- loop's among them, and native passthrough). A client key sends a reasoning
-- item back, as OpenAI wrote it or in lmgw's carrier, only with the encrypted
-- content it was given under that id, and a carrier only to the row that
-- answered with it. NULL for an item without one, and for every row from
-- before this migration.
ALTER TABLE native_response_items ADD COLUMN encrypted_sha256 TEXT;
