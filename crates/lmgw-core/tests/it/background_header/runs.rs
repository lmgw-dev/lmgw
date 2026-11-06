//! A `/v1/responses` run sent with `x-lmgw-background: on` (per-request
//! background design §7, §9): its tools' model calls are the run's traffic —
//! a `docs__query`'s embedder is admitted as a guest, not as the owner — and
//! closing its stream ends the run at once, a tool call in flight included,
//! so its claim goes with it. The owner's streamed run is not ended so: its
//! tool call in flight finishes, and no turn follows it (§11.16).

use lmgw_core::config::{AuxKind, AUX_UPSTREAM_NAME};
use lmgw_core::runtime::registry::Origin;
use lmgw_core::store::NewLocalModel;
use quickdoc_core::embed::{EmbedIdentity, FixtureEmbedder};
use quickdoc_core::store::{self as qstore, NewChunk, NewCorpus};
use serde_json::{json, Value};

use super::*;
use crate::support::gpu_world::GIB;
use crate::support::mcp_stub::{held_call_stub, register_timing_out_after};

/// A public chat row on `<id>.gguf` whose owner-set capabilities say it
/// calls tools (the sparse weights carry no template to read that from).
fn tool_row(id: &str) -> NewLocalModel {
    NewLocalModel {
        fallback: None,
        capabilities_override: Some(json!({"capabilities": {
            "task": "chat",
            "endpoints": ["/v1/chat/completions"],
            "input_modalities": ["text"],
            "tool_calls": {"kind": "native"},
            "source": "owner",
        }})),
        ..with_fallback(id, "")
    }
}

/// `model`'s first answer calls `tool` with `args`; with its result it
/// answers.
fn calls(g: &Gpu, model: &str, tool: &str, args: Value) {
    let mut w = g.world();
    w.calls_tool.insert(model.into(), tool.into());
    w.tool_args.insert(model.into(), args.to_string());
}

/// `axum@0.8`, pinned to the aux embedder `emb` (three dimensions, what the
/// world's containers answer), with one embedded chunk.
async fn corpus_on_emb(g: &Gpu) -> String {
    const DIMS: usize = 3;
    let state = &g.state;
    let cid = qstore::insert_corpus(
        &state.corpus,
        &NewCorpus {
            library: "axum".into(),
            version: "0.8".into(),
            status: "ready".into(),
            embed: EmbedIdentity::new(AUX_UPSTREAM_NAME, "emb", DIMS),
            ingest_model: "ingest-model".into(),
            ingest_prompt_version: "v1".into(),
            crawl_date: "2026-08-29T10:00:00Z".into(),
            source_kind: "markdown".into(),
        },
    )
    .await
    .unwrap();
    let root = "https://docs.rs/axum";
    let sid = qstore::insert_source(&state.corpus, cid, root, "markdown", &[])
        .await
        .unwrap();
    let url = format!("{root}/guide");
    let (did, _) = qstore::upsert_document(&state.corpus, sid, &url, "hash")
        .await
        .unwrap();
    let mut chunk = NewChunk::new(cid, did, (0, 80), "Router::new() attaches handlers.");
    chunk.heading_path = "Routing".into();
    chunk.embedding = Some(FixtureEmbedder::new(DIMS).embed_one("Routing"));
    qstore::insert_chunks(&state.corpus, &url, DIMS, &[chunk])
        .await
        .unwrap();
    "axum@0.8".into()
}

/// A guest's run joins the owner's loaded chat model, and its model calls a
/// `docs__query` whose corpus is pinned to a cold embedder. The embedder
/// does not fit beside the owner's models: as the run's traffic it defers
/// (pinned, it never takes a fallback's vectors) — the owner's idle model is
/// not evicted for it, as it was when the docs tool embedded as the owner.
#[tokio::test]
async fn a_guest_run_s_docs_query_embeds_as_a_guest_and_defers() {
    let (g, gw) = world(24 * GIB, 4).await;
    g.row(tool_row("m"), 8 * GIB).await;
    g.model("x", 14 * GIB).await;
    g.aux("emb", AuxKind::Embed, 6 * GIB).await;
    owner_loads(&g, "x").await;
    owner_loads(&g, "m").await;
    let corpus = corpus_on_emb(&g).await;
    calls(
        &g,
        "m",
        "docs__query",
        json!({"corpus_id": corpus, "query": "routing"}),
    );

    // Streamed: the world's containers call a tool on a streamed answer.
    let body = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .header(BACKGROUND, "on")
        .json(&json!({
            "model": "m",
            "input": "how do I route?",
            "stream": true,
            "tools": [{"type": "mcp", "server_label": "docs", "require_approval": "never"}],
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("response.completed"), "{body}");
    assert!(
        body.contains("\"name\":\"docs__query\""),
        "the tool was called: {body}"
    );

    let row = newest_log(&g, "embed/emb").await;
    assert_eq!(row.error_kind.as_deref(), Some("gpu_deferred"), "{row:?}");
    assert!(
        g.stops().is_empty(),
        "the owner's idle model was not evicted"
    );
    assert_eq!(g.runs(), ["x", "m"], "the embedder was not started");
    assert_eq!(view(&g, "m").unwrap().owner, Origin::Owner);
}

/// A guest's streamed run sits in a tool call that never answers, on a
/// model it started; a foreground request needs that model's room and
/// waits. The client closes its stream: the run ends then and there — not
/// when the tool call returns, or its next frame fails — its claim goes, and
/// the waiting request is served at once, the guest's idle model evicted.
#[tokio::test]
async fn closing_a_guest_s_stream_mid_tool_call_frees_its_claim_at_once() {
    let (g, gw) = world_with_queue(24 * GIB, 3, 30).await;
    g.row(tool_row("bg"), 16 * GIB).await;
    g.model("fg", 16 * GIB).await;
    let (stub, _held) = held_call_stub().await;
    register_timing_out_after(&g.state, "alpha", "a", &stub.url, true, None, 120_000).await;
    calls(&g, "bg", "a__echo", json!({"text": "slow"}));

    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .header(BACKGROUND, "on")
        .json(&json!({
            "model": "bg",
            "input": "hi",
            "stream": true,
            "tools": [{"type": "mcp", "server_label": "alpha", "require_approval": "never"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let body = resp.bytes_stream();
    stub.wait_calls(1).await;
    assert_eq!(view(&g, "bg").unwrap().owner, Origin::Background);

    let waiting = tokio::spawn({
        let gw = gw.clone();
        async move { chat(&gw, "fg", None).await }
    });
    let runtime = g.state.runtime();
    until("the foreground request waits for room", || {
        runtime.draining_for_owner()
    })
    .await;
    assert!(!waiting.is_finished(), "the foreground request waits");

    drop(body);
    let served = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("served once the run ended, not when the tool call or the queue timed out")
        .unwrap();
    answered_locally(served, "fg").await;
    assert_eq!(g.stops(), ["bg"], "the guest's idle model made room");
}

/// The owner's streamed run sits in a tool call when its client closes the
/// stream: the call is not cancelled (§11.16) — it runs to its end, writes
/// its row with its real result, and the stored response carries that
/// result — and no turn follows it. Its model call stops at the hang-up
/// (`client_hangup::responses`).
#[tokio::test]
async fn closing_the_owner_s_stream_mid_tool_call_lets_the_call_finish() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.row(tool_row("m"), 8 * GIB).await;
    let (stub, held) = held_call_stub().await;
    register_timing_out_after(&g.state, "alpha", "a", &stub.url, true, None, 120_000).await;
    calls(&g, "m", "a__echo", json!({"text": "slow"}));

    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({
            "model": "m",
            "input": "hi",
            "stream": true,
            "tools": [{"type": "mcp", "server_label": "alpha", "require_approval": "never"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let body = resp.bytes_stream();
    stub.wait_calls(1).await;
    assert_eq!(view(&g, "m").unwrap().owner, Origin::Owner);

    drop(body);
    // Time for a hang-up cancel to land, were the owner's run given one.
    tokio::time::sleep(Duration::from_millis(300)).await;
    held.notify_one();

    let row = mcp_row(&g, "a__echo").await;
    assert!(row.status < 400, "the call finished: {row:?}");
    let messages = stored_messages(&g).await;
    assert!(
        messages.contains("echo: slow"),
        "the call's real result is stored: {messages}"
    );
    // No turn after it (2026-10-24): the model answered once, the turn
    // that called the tool.
    assert_eq!(g.world().chats, ["m"], "no turn started after the hang-up");
    // Ended by the cancel, as a hang-up during a model call is.
    assert_eq!(stored_incomplete_reason(&g).await, "canceled");
}

/// The owner's run made three calls to run one after the other
/// (`parallel_tool_calls: false`) and sits in the first when its client
/// closes the stream: the first finishes with its row and its real result,
/// the other two are never started — the stored response closes them as
/// never made — and no turn follows.
#[tokio::test]
async fn closing_the_owner_s_stream_mid_sequential_batch_starts_no_further_call() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.row(tool_row("m"), 8 * GIB).await;
    let (stub, held) = held_call_stub().await;
    register_timing_out_after(&g.state, "alpha", "a", &stub.url, true, None, 120_000).await;
    calls(&g, "m", "a__echo", json!({"text": "slow"}));
    g.world().tool_calls_n.insert("m".into(), 3);

    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({
            "model": "m",
            "input": "hi",
            "stream": true,
            "parallel_tool_calls": false,
            "tools": [{"type": "mcp", "server_label": "alpha", "require_approval": "never"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let body = resp.bytes_stream();
    stub.wait_calls(1).await;

    drop(body);
    // Time for the hang-up to land while the first call is in flight.
    tokio::time::sleep(Duration::from_millis(300)).await;
    held.notify_one();

    let row = mcp_row(&g, "a__echo").await;
    assert!(row.status < 400, "the first call finished: {row:?}");
    let messages = stored_messages(&g).await;
    assert!(
        messages.contains("echo: slow"),
        "the first call's real result is stored: {messages}"
    );
    assert_eq!(
        messages
            .matches("not run: the turn ended before this call was made")
            .count(),
        2,
        "the other two are closed as never made: {messages}"
    );
    // A call started after the hang-up would have reached the server by now.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(stub.calls().len(), 1, "no call started after the hang-up");
    assert_eq!(g.world().chats, ["m"], "no turn started after the hang-up");
    assert_eq!(stored_incomplete_reason(&g).await, "canceled");
}

/// The request-log row of the tool call `tool`, waited for.
async fn mcp_row(g: &Gpu, tool: &str) -> store::RequestLogRow {
    for _ in 0..500 {
        let rows = store::query_logs(
            &g.state.db,
            &store::LogFilter {
                limit: 50,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if let Some(row) = rows
            .into_iter()
            .find(|r| r.mcp_tool.as_deref() == Some(tool))
        {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no request-log row for the tool call '{tool}'");
}

/// The stored conversation of the one response this world made, waited
/// for: it is written once the run has ended.
async fn stored_messages(g: &Gpu) -> String {
    stored(g).await.messages
}

/// Why the one response this world made is incomplete, as stored.
async fn stored_incomplete_reason(g: &Gpu) -> Value {
    let body: Value = serde_json::from_str(&stored(g).await.body).unwrap();
    body["incomplete_details"]["reason"].clone()
}

/// The one response this world made, waited for: it is written once the
/// run has ended.
async fn stored(g: &Gpu) -> store::StoredResponse {
    for _ in 0..500 {
        let chains = store::list_response_chains(&g.state.db, 10).await.unwrap();
        if let Some(chain) = chains.first() {
            return store::get_response(&g.state.db, &chain.head_id, store::ResponseReader::Owner)
                .await
                .unwrap()
                .expect("the chain's head is stored");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no stored response");
}
