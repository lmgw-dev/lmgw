//! `GET /api/responses` and `GET /api/responses/chain` over the real router:
//! each live answer is read into the document's type and written back equal
//! (`common::round_trips`), on an empty store, on chains of every shape and
//! past the 200 the list shows. The key sets are pinned where the type alone
//! cannot say.

use lmgw_api_types as dto;
use lmgw_core::state::AppState;
use lmgw_core::store::{self, StoredResponse};
use serde_json::{json, Value};

use crate::common;
use crate::models_full_wire::{get, keys, read, set};

fn stored(id: &str, chain: &str, prev: Option<&str>, body: &str) -> StoredResponse {
    StoredResponse {
        id: id.into(),
        chain_id: chain.into(),
        previous_response_id: prev.map(String::from),
        model: "my-model".into(),
        status: "completed".into(),
        body: body.into(),
        input_items: "[]".into(),
        messages: "[]".into(),
        pending: None,
        input_tokens: Some(3),
        output_tokens: Some(4),
        created_at: String::new(),
    }
}

#[tokio::test]
async fn the_stored_conversations_are_the_documented_types() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;

    // Nothing stored: an empty list, the rules in a sentence, and a chain that is not there.
    let live = read(&gw, "/api/responses").await;
    let index: dto::ResponsesIndex = common::round_trips("responses, empty", &live);
    assert!(index.chains.is_empty() && !index.capped);
    assert!(index.store_enabled);
    assert_eq!(
        index.rules,
        "evicted after 7 idle day(s), keeping at most 500"
    );
    let (status, refused) = get(&gw, "/api/responses/chain?id=nope").await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["message"], "no such conversation");

    let rich = json!({
        "output": [
            {"type": "message", "content": [
                {"type": "output_text", "text": "Hello "}, {"type": "output_text", "text": "world"}]},
            {"type": "mcp_call"}, {"type": "mcp_call"}, {"type": "reasoning"}, {"nothing": 1}
        ]
    })
    .to_string();
    let pending = json!([
        {"approval_id": "ap1", "call_id": "c1", "name": "tool_a", "args": {}, "server_label": "s", "needs_approval": true},
        {"approval_id": "ap2", "call_id": "c2", "name": "tool_b", "args": {}, "server_label": "s", "needs_approval": false}
    ])
    .to_string();
    let mut waiting = stored("r2", "r1", Some("r1"), &rich);
    waiting.pending = Some(pending);
    let mut unread = stored(
        "big",
        "big",
        None,
        &format!("{{\"pad\":\"{}\"}}", "x".repeat(3_000_000)),
    );
    unread.input_tokens = None;
    unread.output_tokens = None;
    for r in [
        stored("r1", "r1", None, &rich),
        waiting,
        stored("junk", "junk", None, "not json at all"),
        unread,
    ] {
        store::insert_response(&state.db, &r).await.unwrap();
    }

    let live = read(&gw, "/api/responses").await;
    let index: dto::ResponsesIndex = common::round_trips("responses", &live);
    assert_eq!((index.total_chains, index.total_responses), (3, 4));
    let row = |id: &str| index.chains.iter().find(|c| c.chain_id == id).unwrap();
    assert_eq!(row("r1").responses, 2);
    assert!(row("r1").awaiting_approval && !row("big").awaiting_approval);
    assert_eq!((row("r1").input_tokens, row("big").input_tokens), (6, 0));
    assert!(row("big").size.ends_with(" MiB"), "{}", row("big").size);
    assert!(row("junk").size.ends_with(" B"), "{}", row("junk").size);
    assert_eq!(
        keys(&live["chains"][0]),
        set(&[
            "awaiting_approval",
            "chain_id",
            "first_at",
            "head_id",
            "input_tokens",
            "last_at",
            "model",
            "output_tokens",
            "responses",
            "size",
            "status",
        ])
    );

    let live = read(&gw, "/api/responses/chain?id=r1").await;
    let chain: dto::ChainDetail = common::round_trips("chain", &live);
    assert_eq!(chain.chain_id, "r1");
    let first = &chain.responses[0];
    assert_eq!(
        chain
            .responses
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["r1", "r2"]
    );
    assert_eq!(first.outline, "message, 2 mcp_call, reasoning, ?");
    assert_eq!(first.text, "Hello world");
    assert!(first.pending.is_empty());
    assert_eq!(chain.responses[1].pending, ["tool_a (ap1)"]);
    assert_eq!(
        first.body,
        serde_json::to_string_pretty(&serde_json::from_str::<Value>(&rich).unwrap()).unwrap()
    );

    // A body that is not JSON is `null`, pretty-printed; no tokens are `null`.
    let live = read(&gw, "/api/responses/chain?id=junk").await;
    let chain: dto::ChainDetail = common::round_trips("chain, junk body", &live);
    assert_eq!(
        (
            chain.responses[0].body.as_str(),
            chain.responses[0].outline.as_str()
        ),
        ("null", "")
    );
    let live = read(&gw, "/api/responses/chain?id=big").await;
    let chain: dto::ChainDetail = common::round_trips("chain, no tokens", &live);
    assert_eq!(
        (
            chain.responses[0].input_tokens,
            chain.responses[0].output_tokens
        ),
        (None, None)
    );
    assert!(live["responses"][0]["input_tokens"].is_null());
}

/// More chains than the list shows: it is capped at 200 and says so.
#[tokio::test]
async fn a_list_of_more_than_200_conversations_says_it_is_capped() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;
    for i in 0..201 {
        let id = format!("c{i:03}");
        store::insert_response(&state.db, &stored(&id, &id, None, "{}"))
            .await
            .unwrap();
    }
    let live = read(&gw, "/api/responses").await;
    let index: dto::ResponsesIndex = common::round_trips("responses, capped", &live);
    assert_eq!(
        (index.chains.len(), index.total_chains, index.capped),
        (200, 201, true)
    );
}
