//! The loop's GPU claim around a picture (design §4.3): on a 10 GiB card a
//! local chat model of 8 GiB and an image row of 6 GiB never fit together,
//! and `vram.queue_timeout_seconds` is 0 — an admission waits for room as
//! long as it takes. The chat model asks for `image__generate`; its loop
//! lets its claim go, the image row's admission evicts the chat model, the
//! picture is drawn, and the next model call takes the room back. With the
//! claim held through the call it was a circular wait: the loop hung for
//! good, held and silent.
//!
//! The Chat, a `/v1/responses` run (review finding 1) and a candidate alias
//! whose model the picture evicted (finding 3: the alias may not start it
//! again, so the next call is picked again through the gate).

use std::time::Duration;

use lmgw_core::runtime::descriptor::model_runtime;
use lmgw_core::runtime::lifecycle::acquire_spec;
use lmgw_core::runtime::Class;
use lmgw_core::store::{NewCandidateAlias, NewLocalModel};
use serde_json::{json, Value};

use crate::common::{serve, Gw};
use crate::support::gpu_world::{Gpu, ANSWER, GIB};

/// The owner's capability facts for a chat row that calls tools: the
/// sparse weights files carry no GGUF header to read them from.
fn calls_tools() -> Value {
    json!({"capabilities": {
        "task": "chat",
        "endpoints": ["/v1/chat/completions"],
        "input_modalities": ["text"],
        "tool_calls": {"kind": "native"},
        "source": "owner",
    }})
}

fn row(id: &str) -> NewLocalModel {
    NewLocalModel {
        model_id: id.into(),
        gguf_path: format!("{id}.gguf"),
        params: Default::default(),
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        fallback: None,
        capabilities_override: Some(calls_tools()),
        ladder: vec![],
    }
}

/// The card, the chat row `gemma` that draws once and then answers, and
/// the image row `z`.
pub(super) async fn world() -> Gpu {
    let g = Gpu::new(10 * GIB, 6, 0).await;
    g.row(row("gemma"), 8 * GIB).await;
    g.image("z", 6 * GIB).await;
    draws(&g, "gemma");
    g
}

/// `model` asks for one picture of `z`, and answers once it has it.
fn draws(g: &Gpu, model: &str) {
    let mut w = g.world();
    w.thinking.insert(model.into());
    w.calls_tool.insert(model.into(), "image__generate".into());
    w.tool_args.insert(
        model.into(),
        json!({"model": "image/z", "prompt": "a red square"}).to_string(),
    );
}

/// A Chat thread on `alias` with the `image` toolset, sent one message:
/// the turn's whole SSE body.
pub(super) async fn chat_turn(gw: &Gw, alias: &str) -> String {
    let t: Value = gw
        .client()
        .post(format!("{gw}/chat/api/threads"))
        .json(&json!({"model_alias": alias}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = t["id"].as_i64().unwrap();
    gw.client()
        .post(format!("{gw}/chat/api/threads/{tid}/settings"))
        .json(&json!({"mcp_tools": [{"server_label": "image"}]}))
        .send()
        .await
        .unwrap();
    let send = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/send"))
        .json(&json!({"content": "draw me a red square"}))
        .send();
    tokio::time::timeout(Duration::from_secs(30), async {
        send.await.unwrap().text().await.unwrap()
    })
    .await
    .expect("the turn ends: no circular wait on the GPU")
}

#[tokio::test]
async fn a_chat_turn_s_picture_gets_the_chat_model_s_room_and_gives_it_back() {
    let g = world().await;
    let gw = serve(g.state.clone()).await;
    let body = chat_turn(&gw, "gemma").await;
    assert!(body.contains(ANSWER), "{body}");
    assert_eq!(g.world().images, ["z"], "the picture was drawn");
    // `gemma` evicted for the picture, `z` for the next model call.
    assert_eq!(g.runs(), ["gemma", "z", "gemma"]);
    assert_eq!(g.stops(), ["gemma", "z"]);
}

#[tokio::test]
async fn a_responses_run_s_picture_gets_the_chat_model_s_room_and_gives_it_back() {
    let g = world().await;
    let gw = serve(g.state.clone()).await;
    let send = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({
            "model": "gemma",
            "input": "draw me a red square",
            "stream": true,
            "tools": [{"type": "mcp", "server_label": "image", "require_approval": "never"}],
        }))
        .send();
    let body = tokio::time::timeout(Duration::from_secs(30), async {
        send.await.unwrap().text().await.unwrap()
    })
    .await
    .expect("the run ends: no circular wait on the GPU");
    assert!(body.contains("response.completed"), "{body}");
    assert!(body.contains(ANSWER), "{body}");
    assert_eq!(g.world().images, ["z"], "the picture was drawn");
    assert_eq!(g.runs(), ["gemma", "z", "gemma"]);
    assert_eq!(g.stops(), ["gemma", "z"]);
}

/// A candidate alias whose alternate `gemma` answered because it was
/// loaded: the picture evicts it, and the alias may not start it again
/// (`Restart::No`). The next model call is picked again — the primary
/// `big`, started through admission — instead of failing the turn.
#[tokio::test]
async fn a_candidate_lost_to_a_picture_is_picked_again() {
    let g = world().await;
    g.row(row("big"), 8 * GIB).await;
    g.world().thinking.insert("big".into());
    g.candidate(NewCandidateAlias {
        alias: "pick".into(),
        candidates: vec!["big".into(), "gemma".into()],
        background: false,
        fallback: None,
        capabilities_disabled: vec![],
        capabilities_enabled: vec!["tool_calls".into()],
        enabled: true,
        notes: String::new(),
    })
    .await;
    // `gemma` loaded, so the alias's walk joins it.
    let snap = g.state.snapshot();
    let rt = model_runtime(&snap, Class::Chat, "gemma").unwrap();
    drop(
        g.state
            .runtime()
            .acquire(&acquire_spec(&g.state, &snap, &rt))
            .await
            .unwrap(),
    );
    let gw = serve(g.state.clone()).await;

    let body = chat_turn(&gw, "pick").await;
    assert!(body.contains(ANSWER), "{body}");
    assert!(!body.contains("candidate_lost"), "{body}");
    assert_eq!(g.world().images, ["z"]);
    let chats = g.world().chats.clone();
    assert_eq!(
        chats.last().map(String::as_str),
        Some("big"),
        "the call after the picture went to the primary: {chats:?}"
    );
    assert_eq!(g.runs(), ["gemma", "z", "big"]);
}
