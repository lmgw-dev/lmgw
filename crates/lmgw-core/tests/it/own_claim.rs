//! A request that leaves its model for its fallback keeps its claim on it
//! (openai egress design §14.3): the fallback's admission may evict the
//! model it leaves only when that claim is the model's one user
//! (`vram::leaving`). A loop's next turn finds the model still there when
//! nothing evicted it; a claim shared by a second turn, or a second request
//! on the model, keeps it.
//!
//! The climb's hand-over is `vram_admission/ladder_http.rs`'s
//! `a_local_fallback_that_fits_only_without_the_ladder_takes_its_room`.

use std::time::Duration;

use lmgw_core::error::GatewayError;
use lmgw_core::ir::{ChatRequest, Params};
use lmgw_core::store::{NewCandidateAlias, NewLocalModel};
use lmgw_core::vram;
use serde_json::json;

use crate::common::{self, Gw};
use crate::responses_api::{mcp_stub, register_mcp};
use crate::support::gpu_world::{Gpu, ANSWER, GIB};

/// A background candidate alias falling back to `fallback`; `tools`: it
/// enables tool calls, which every candidate it uses then needs.
fn candidates(name: &str, list: &[&str], fallback: &str, tools: bool) -> NewCandidateAlias {
    NewCandidateAlias {
        alias: name.into(),
        candidates: list.iter().map(|c| c.to_string()).collect(),
        background: true,
        fallback: Some(fallback.into()),
        capabilities_disabled: vec![],
        capabilities_enabled: if tools {
            vec!["tool_calls".into()]
        } else {
            vec![]
        },
        enabled: true,
        notes: String::new(),
    }
}

/// A public chat row that calls tools, on `<id>.gguf`: the sparse weights
/// files carry no GGUF header to read that from.
fn tool_row(id: &str) -> NewLocalModel {
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
        capabilities_override: Some(json!({"capabilities": {
            "task": "chat",
            "endpoints": ["/v1/chat/completions"],
            "input_modalities": ["text"],
            "tool_calls": {"kind": "native"},
            "source": "owner",
        }})),
        ladder: vec![],
    }
}

async fn chat(gw: &Gw, model: &str) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
}

fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

/// A guest's candidate refuses the prompt and is picked again; nothing else
/// is loaded, so the alias fallback answers — and it fits only once the
/// candidate goes. The request's own claim on the candidate is its only
/// user, so the fallback's admission evicts it rather than deferring behind
/// the request itself.
#[tokio::test]
async fn a_repicked_candidates_own_claim_makes_room_for_the_alias_fallback() {
    let g = Gpu::new(16 * GIB, 2, 2).await;
    let gw = common::serve(g.state.clone()).await;
    g.model("p", 8 * GIB).await;
    g.model("b", 12 * GIB).await;
    g.candidate(candidates("jobs", &["p"], "b", false)).await;
    // `p` started for background traffic, and idle: a guest's admission
    // may evict it.
    match vram::start_background(&g.state, &g.route("p"), "jobs")
        .await
        .unwrap()
    {
        vram::BackgroundStart::Started(hold) => drop(hold),
        vram::BackgroundStart::Blocked(why) => panic!("blocked by: {why}"),
    }
    assert_eq!(g.runs(), ["p"]);

    g.world().refuse_context.insert("p".into());
    let resp = chat(&gw, "jobs").await;
    let fell_to = header(&resp, "x-lmgw-fallback");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(fell_to.as_deref(), Some("b"), "{body}");
    assert_eq!(g.stops(), ["p"], "the candidate made room");
    assert_eq!(g.runs(), ["p", "b"]);
}

/// A guest's `/v1/responses` run on an alternate it uses only while it is
/// loaded (`Restart::No`): its first model call is refused and handed to
/// the alias fallback, which fits beside the alternate and calls a tool.
/// The run's next model call is on its own claim again, and finds the
/// alternate still loaded — no `candidate_lost`, nothing started or stopped
/// for it.
#[tokio::test]
async fn a_loops_next_turn_after_a_hand_over_finds_its_model_still_there() {
    let g = Gpu::new(24 * GIB, 3, 2).await;
    let gw = common::serve(g.state.clone()).await;
    g.row(tool_row("big"), 20 * GIB).await;
    g.row(tool_row("p"), 8 * GIB).await;
    g.row(tool_row("b"), 6 * GIB).await;
    g.candidate(candidates("jobs", &["big", "p"], "b", true))
        .await;
    let (url, calls) = mcp_stub().await;
    register_mcp(&g.state, "stub", "stub", &url).await;
    // The owner's `p`, loaded and idle: the guest joins it, and cannot
    // start `big` beside it.
    drop(
        vram::admit(&g.state, &g.route("p"), "p")
            .await
            .unwrap()
            .expect("a local model"),
    );
    {
        let mut w = g.world();
        w.refuse_context_times.insert("p".into(), 1);
        w.calls_tool.insert("b".into(), "stub__echo".into());
        w.tool_args
            .insert("b".into(), json!({"text": "x"}).to_string());
        w.thinking.insert("p".into());
    }

    let send = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({
            "model": "jobs",
            "input": "hi",
            "stream": true,
            "tools": [{"type": "mcp", "server_label": "stub", "require_approval": "never"}],
        }))
        .send();
    let body = tokio::time::timeout(Duration::from_secs(30), async {
        send.await.unwrap().text().await.unwrap()
    })
    .await
    .expect("the run ends");
    assert!(body.contains("response.completed"), "{body}");
    assert!(body.contains(ANSWER), "{body}");
    assert!(!body.contains("candidate_lost"), "{body}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(g.world().chats, ["b", "p"], "the next turn was on `p`");
    assert_eq!(g.runs(), ["p", "b"], "nothing restarted");
    assert!(g.stops().is_empty(), "{:?}", g.stops());
}

/// What a fallback's admission inside [`vram::leaving`] may take, on a card
/// where `b` fits only once `a` is gone: `a` while the leaving claim is its
/// only user — not while another model call's turn shares that claim (the
/// leaving send's own has ended), and not while a second request holds a
/// claim of its own on `a`. Those wait, and end at the queue timeout with
/// `a` still loaded.
#[tokio::test]
async fn a_shared_claim_or_a_second_request_keeps_the_model_being_left() {
    let g = Gpu::new(16 * GIB, 3, 1).await;
    g.model("a", 8 * GIB).await;
    g.model("b", 12 * GIB).await;
    let admit = |id: &'static str| {
        let (state, route) = (g.state.clone(), g.route(id));
        async move { vram::admit(&state, &route, id).await }
    };
    let held = admit("a").await.unwrap().expect("a local model");

    // Another call's turn on the claim: `a` is still being used.
    let sibling = held.turn();
    let refused = vram::leaving(&held, admit("b")).await.err();
    assert!(
        matches!(refused, Some(GatewayError::VramQueueTimeout { .. })),
        "{refused:?}"
    );
    drop(sibling);

    // A second request's claim on `a`.
    let other = admit("a").await.unwrap().expect("a local model");
    let refused = vram::leaving(&held, admit("b")).await.err();
    assert!(
        matches!(refused, Some(GatewayError::VramQueueTimeout { .. })),
        "{refused:?}"
    );
    assert!(g.stops().is_empty(), "{:?}", g.stops());
    drop(other);

    // Outside `leaving` the request's own claim keeps `a` too.
    let refused = admit("b").await.err();
    assert!(
        matches!(refused, Some(GatewayError::VramQueueTimeout { .. })),
        "{refused:?}"
    );

    // No turn, the one claim: `a` makes room.
    let b = vram::leaving(&held, admit("b")).await.unwrap();
    assert!(b.is_some());
    assert_eq!(g.stops(), ["a"]);
}

/// A sibling send on a shared claim is a turn from the start of its fit
/// (review V2), not from its send: one whose fit is done — or still
/// counting its prompt — keeps the model a fallback's admission inside
/// [`vram::leaving`] would otherwise take.
#[tokio::test]
async fn a_sibling_turn_counts_from_its_fit() {
    let g = Gpu::new(16 * GIB, 3, 1).await;
    g.model("a", 8 * GIB).await;
    g.model("b", 12 * GIB).await;
    let admit = |id: &'static str| {
        let (state, route) = (g.state.clone(), g.route(id));
        async move { vram::admit(&state, &route, id).await }
    };
    let held = admit("a").await.unwrap().expect("a local model");

    let ir: ChatRequest = serde_json::from_value(json!({
        "model_alias": "a",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
    }))
    .unwrap();
    let mut params = Params::default();
    let route = g.route("a");
    let fitted =
        lmgw_core::gate::fit_chat(&g.state, Some(&held), &route, &ir, &mut params, false, None)
            .await;
    let Ok((sibling, _)) = fitted else {
        panic!("the fit refused");
    };
    let refused = vram::leaving(&held, admit("b")).await.err();
    assert!(
        matches!(refused, Some(GatewayError::VramQueueTimeout { .. })),
        "{refused:?}"
    );
    assert!(g.stops().is_empty(), "{:?}", g.stops());

    // The sibling's response ended: the claim is the leaving request's alone.
    drop(sibling);
    let b = vram::leaving(&held, admit("b")).await.unwrap();
    assert!(b.is_some());
    assert_eq!(g.stops(), ["a"]);
}

/// A card where the left model's room is what the fallback needs: 32 GiB
/// with 4 outside lmgw, per-process attribution on so a rung above lmgw's
/// 28 is outside VRAM (its request's fallback answers), 100 prompt tokens
/// that every ladder's base (`-c 64`) climbs for, and `a`, a ladder whose
/// second rung never fits here.
async fn left_room_world() -> (Gpu, Gw) {
    let g = Gpu::new(32 * GIB, 5, 2).await;
    {
        let mut w = g.world();
        w.attribution = true;
        w.outside = 4 * GIB;
        w.prompt_tokens = 100;
    }
    g.ladder("a", 4 * GIB, &[(30 * GIB, 4096)]).await;
    let gw = common::serve(g.state.clone()).await;
    (g, gw)
}

async fn falls_back(g: &Gpu, owner: &str, to: &str) {
    crate::support::fallbacks::set_fallback(&g.state, "local_models", owner, Some(to)).await;
}

/// An owner's `/v1/responses` run on `a` (review V1): `a`'s climb cannot
/// happen, so the turn goes to `b`, a ladder whose base fits beside `a` and
/// whose climb fits only once `a` is gone (26 GiB; 24 with `a` there). The
/// run still holds its claim on `a` for its next turn, and that claim is
/// `a`'s only user: `b`'s climb, part of the rerouted call, evicts `a`
/// instead of waiting for the queue timeout behind the run's own claim.
#[tokio::test]
async fn an_in_process_fallback_that_climbs_takes_the_room_of_the_model_its_run_left() {
    let (g, gw) = left_room_world().await;
    g.ladder("b", 2 * GIB, &[(26 * GIB, 4096)]).await;
    falls_back(&g, "a", "b").await;

    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": "a", "input": "hi"}))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("ok from b"), "{body}");
    assert!(g.stops().contains(&"a".to_string()), "{:?}", g.stops());
    assert_eq!(g.runs(), ["a", "b", "b"], "b climbed");
}

/// Two hops in one streamed `/v1/responses` turn (review V1): `a` hands
/// over to `b`, whose climb cannot happen either, so `b` hands on to `c` —
/// which fits only once `a` goes (25 GiB: 24 beside `a` alone, 26 beside
/// `b` alone). `c`'s admission runs inside both claims the request left,
/// and `a`'s is still freeable there.
#[tokio::test]
async fn a_second_hand_over_may_take_the_room_of_the_first_model_left() {
    let (g, gw) = left_room_world().await;
    g.ladder("b", 2 * GIB, &[(30 * GIB, 4096)]).await;
    g.model("c", 25 * GIB).await;
    falls_back(&g, "a", "b").await;
    falls_back(&g, "b", "c").await;
    g.world().thinking.insert("c".into());

    let send = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": "a", "input": "hi", "stream": true}))
        .send();
    let body = tokio::time::timeout(Duration::from_secs(30), async {
        send.await.unwrap().text().await.unwrap()
    })
    .await
    .expect("the run ends");
    assert!(body.contains("response.completed"), "{body}");
    assert!(body.contains(ANSWER), "{body}");
    assert!(g.stops().contains(&"a".to_string()), "{:?}", g.stops());
    assert_eq!(g.runs(), ["a", "b", "c"]);
}
