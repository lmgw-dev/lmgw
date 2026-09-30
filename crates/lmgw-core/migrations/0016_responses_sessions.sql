-- §21 stage 2: stored responses, so `previous_response_id` works.
--
-- The Responses API is stateful by design: a client sends one turn and names
-- the response it continues from. Stage 1 refused that field rather than lose
-- the conversation silently; this table is what lets it be honored.
--
-- `messages` is the **IR**, not the wire items. A stored `mcp_call.output` is a
-- flattened string (the Responses schema says so), while the model was given
-- the tool's real blocks — replaying from the wire form would quietly degrade
-- every chained turn. `body` is kept alongside it so `GET /v1/responses/{id}`
-- returns byte-for-byte what the POST returned.
--
-- `chain_id` denormalizes the root of the `previous_response_id` list. GC is
-- chain-aware and needs it: evicting individual responses by age deletes the
-- *root* first (it is the oldest) and orphans a conversation that is still in
-- use. One indexed column turns "evict whole idle chains" into one query.
CREATE TABLE responses (
    id                   TEXT PRIMARY KEY,           -- resp_…
    chain_id             TEXT NOT NULL,              -- id of the chain's first response
    previous_response_id TEXT,                       -- NULL for a chain root
    model                TEXT NOT NULL,              -- the alias the client asked for
    status               TEXT NOT NULL,              -- completed | incomplete | failed
    -- The response object as served, verbatim.
    body                 TEXT NOT NULL,
    -- This turn's own `input` items, for GET /v1/responses/{id}/input_items.
    input_items          TEXT NOT NULL DEFAULT '[]',
    -- The IR conversation as the run ended (JSON array of ir::Message).
    messages             TEXT NOT NULL,
    -- Tool calls the run stopped on: awaiting client approval, or blocked
    -- behind one (a turn's calls cannot be split). NULL when nothing is pending.
    pending              TEXT,
    input_tokens         INTEGER,
    output_tokens        INTEGER,
    created_at           TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Chain listing + GC ("newest activity per chain") and plain age eviction.
CREATE INDEX idx_responses_chain ON responses(chain_id, created_at);
CREATE INDEX idx_responses_created ON responses(created_at);

-- Admin Chat (§21 stage 2): a Chat-tab thread wired to the `lmgw__*` self-admin
-- tools instead of a bare model. Only the flavor differs, so it is a column on
-- the existing table rather than a second one.
ALTER TABLE chat_threads ADD COLUMN kind TEXT NOT NULL DEFAULT 'chat';

-- An agentic turn is more than its final text: it is assistant tool calls and
-- their results, interleaved. Storing only `content` would make the *next* turn
-- forget every tool it just ran. This holds the turn's IR messages (JSON array
-- of ir::Message) so the conversation replays exactly as the loop left it, and
-- so a reopened thread can render the tool calls it made. NULL for plain chat.
ALTER TABLE chat_messages ADD COLUMN ir_messages TEXT;
