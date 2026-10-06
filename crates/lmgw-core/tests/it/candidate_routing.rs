//! The request gate's candidate walk (candidate-aliases design §4.1–4.3,
//! §4.6, §4.7; §12 entries 45–48): which of a candidate alias's models
//! answers a request, the alias fallback, `x-lmgw-candidate`, the facet
//! `400`, and the re-pick a send makes when its candidate cannot take the
//! request.
//!
//! Driven over HTTP through the real router, on `support/gpu_world.rs`: a
//! fake driver whose free memory follows what the fake podman loaded, so an
//! eviction really makes room and a blocked start really means the card is
//! full. The vram half the walk calls — join, the guest's start, draining —
//! is `background_admission.rs`'s; the save-time rules are
//! `candidate_aliases.rs`'s.

use std::sync::Arc;
use std::time::Duration;

use lmgw_core::config::HoldFallbackMode;
use lmgw_core::runtime::registry::{Origin, RuntimeState, RuntimeView};
use lmgw_core::store::{self, NewCandidateAlias, NewLocalModel};
use lmgw_core::vram;
use serde_json::{json, Value};

use crate::common;
use common::{serve, Gw};

use crate::support::gpu_world;
use gpu_world::{Gpu, GIB};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

async fn world(total: u64, containers: usize) -> (Gpu, Gw) {
    let g = Gpu::new(total, containers, 2).await;
    let gw = serve(g.state.clone()).await;
    (g, gw)
}

/// A candidate alias as stored: `fallback` `None` is mode `none`, `enabled`
/// the facets it enables.
fn alias(
    name: &str,
    candidates: &[&str],
    background: bool,
    fallback: Option<&str>,
    enabled: &[&str],
) -> NewCandidateAlias {
    NewCandidateAlias {
        alias: name.into(),
        candidates: candidates.iter().map(|c| c.to_string()).collect(),
        background,
        fallback_mode: if fallback.is_some() {
            HoldFallbackMode::Alias
        } else {
            HoldFallbackMode::None
        },
        fallback: fallback.map(str::to_string),
        capabilities_disabled: vec![],
        capabilities_enabled: enabled.iter().map(|f| f.to_string()).collect(),
        enabled: true,
        notes: String::new(),
    }
}

/// A complete capability object — the sparse weights files carry no GGUF
/// header, so an override has to state everything, `task` included.
fn caps(vision: bool) -> Value {
    let mut modalities = vec!["text"];
    if vision {
        modalities.push("image");
    }
    json!({"capabilities": {
        "task": "chat",
        "endpoints": ["/v1/chat/completions"],
        "input_modalities": modalities,
        "vision": vision,
        "source": "owner",
    }})
}

/// A public chat row on `<id>.gguf` with the owner's capability facts.
fn row(id: &str, capabilities: Option<Value>) -> NewLocalModel {
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
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: capabilities,
        ladder: vec![],
    }
}

/// Load `model` as the owner would — a request for its own name — and leave
/// it resident and idle. Returns its port.
async fn owner_loads(g: &Gpu, model: &str) -> u16 {
    let hold = vram::admit(&g.state, &g.route(model), model)
        .await
        .unwrap()
        .expect("a local model");
    hold.port()
}

async fn chat(gw: &Gw, model: &str) -> reqwest::Response {
    chat_body(
        gw,
        json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await
}

async fn chat_body(gw: &Gw, body: Value) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn header<'r>(resp: &'r reqwest::Response, name: &str) -> Option<&'r str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

fn view(g: &Gpu, model: &str) -> Option<RuntimeView> {
    g.state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == model)
}

/// The newest request-log row for `alias` (the record is written as the
/// response goes out, so it is waited for).
async fn newest_log(g: &Gpu, alias: &str) -> store::RequestLogRow {
    for _ in 0..200 {
        let rows = store::query_logs(
            &g.state.db,
            &store::LogFilter {
                alias: Some(alias.into()),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if let Some(row) = rows.into_iter().next() {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no request-log row for '{alias}'");
}

/// An answer from a local candidate: 200, `x-lmgw-candidate` names it, no
/// fallback header, the container that answered was that model's, and the
/// log row keeps the alias as requested and the candidate as served.
async fn answered_by(g: &Gpu, resp: reqwest::Response, alias: &str, model: &str) {
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    assert_eq!(header(&resp, "x-lmgw-candidate"), Some(model));
    assert_eq!(header(&resp, "x-lmgw-fallback"), None);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(
        v["choices"][0]["message"]["content"],
        format!("ok from {model}")
    );
    assert_eq!(v["model"], alias, "the body echoes the requested name");
    let row = newest_log(g, alias).await;
    assert_eq!(row.upstream_model.as_deref(), Some(model), "{row:?}");
    assert_eq!(row.fallback_reason, None);
}

/// An answer from the cloud fallback `fb`, for `reason`: no candidate header.
async fn answered_by_fallback(
    g: &Gpu,
    resp: reqwest::Response,
    alias: &str,
    fb: &str,
    reason: &str,
) {
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some(fb));
    assert_eq!(header(&resp, "x-lmgw-fallback-reason"), Some(reason));
    assert_eq!(header(&resp, "x-lmgw-candidate"), None);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "from the cloud");
    let row = newest_log(g, alias).await;
    assert_eq!(row.fallback_reason.as_deref(), Some(reason), "{row:?}");
}

async fn hold_on(g: &Gpu) {
    let mut s = g.state.snapshot().settings.clone();
    s.hold.active = true;
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
}

// ---------------------------------------------------------------------------
// §7 item 4: without background
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_owner_alias_uses_its_loaded_primary() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("p", 8 * GIB).await;
    g.model("a", 8 * GIB).await;
    g.candidate(alias("writer", &["p", "a"], false, None, &[]))
        .await;
    owner_loads(&g, "p").await;
    owner_loads(&g, "a").await;

    answered_by(&g, chat(&gw, "writer").await, "writer", "p").await;
    assert_eq!(g.runs(), ["p", "a"], "nothing started for the request");
}

/// A loaded alternate answers instead of evicting anything to make room for
/// the primary: no eviction, no podman call.
#[tokio::test]
async fn an_owner_alias_uses_a_loaded_alternate_instead_of_loading_its_primary() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("p", 8 * GIB).await;
    g.model("a", 8 * GIB).await;
    g.candidate(alias("writer", &["p", "a"], false, None, &[]))
        .await;
    owner_loads(&g, "a").await;

    answered_by(&g, chat(&gw, "writer").await, "writer", "a").await;
    assert_eq!(g.runs(), ["a"], "the primary was not started");
    assert!(g.stops().is_empty());
}

/// Nothing loaded: the primary starts with normal admission, which evicts
/// an idle unrelated model — and the alternate is never started by the
/// alias.
#[tokio::test]
async fn an_owner_alias_loads_its_primary_by_evicting_and_never_starts_an_alternate() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 16 * GIB).await;
    g.model("p", 12 * GIB).await;
    g.model("a", 4 * GIB).await;
    g.candidate(alias("writer", &["p", "a"], false, None, &[]))
        .await;
    owner_loads(&g, "x").await;

    answered_by(&g, chat(&gw, "writer").await, "writer", "p").await;
    assert_eq!(g.stops(), ["x"], "the idle unrelated model made room");
    assert_eq!(g.runs(), ["x", "p"], "the alternate was never started");
    assert_eq!(view(&g, "p").unwrap().owner, Origin::Owner);
}

/// Loading the primary fails as a direct request's would — here a model no
/// card could hold, `vram_too_large`, never the fallback — and the refusal
/// names the candidate it was about.
#[tokio::test]
async fn an_owner_alias_names_its_primary_on_an_admission_error() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("p", 30 * GIB).await;
    g.cloud("cloud", None).await;
    g.candidate(alias("writer", &["p"], false, Some("cloud"), &[]))
        .await;

    let resp = chat(&gw, "writer").await;
    assert_eq!(header(&resp, "x-lmgw-candidate"), Some("p"));
    assert_eq!(header(&resp, "x-lmgw-fallback"), None);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "vram_too_large", "{v}");
    assert!(g.runs().is_empty());
}

// ---------------------------------------------------------------------------
// §7 items 5–7: with background
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_background_alias_uses_the_owners_loaded_primary() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("p", 8 * GIB).await;
    g.model("a", 8 * GIB).await;
    g.candidate(alias("jobs", &["p", "a"], true, None, &[]))
        .await;
    owner_loads(&g, "p").await;

    answered_by(&g, chat(&gw, "jobs").await, "jobs", "p").await;
    assert_eq!(
        view(&g, "p").unwrap().owner,
        Origin::Owner,
        "still the owner's"
    );
}

/// "Load primary if possible": a primary that fits the free VRAM is started
/// for the guest even though an alternate is loaded, and is the guest's.
#[tokio::test]
async fn a_background_alias_starts_its_primary_into_free_vram_even_with_an_alternate_loaded() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("p", 8 * GIB).await;
    g.model("a", 8 * GIB).await;
    g.candidate(alias("jobs", &["p", "a"], true, None, &[]))
        .await;
    owner_loads(&g, "a").await;

    answered_by(&g, chat(&gw, "jobs").await, "jobs", "p").await;
    assert_eq!(g.runs(), ["a", "p"]);
    assert_eq!(view(&g, "p").unwrap().owner, Origin::Background);
}

/// The primary would need the owner's idle model evicted: blocked, and the
/// loaded alternate answers. Nothing of the owner's is stopped.
#[tokio::test]
async fn a_background_alias_whose_primary_is_blocked_uses_a_loaded_alternate() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 16 * GIB).await;
    g.model("p", 12 * GIB).await;
    g.model("a", 4 * GIB).await;
    g.candidate(alias("jobs", &["p", "a"], true, None, &[]))
        .await;
    owner_loads(&g, "x").await;
    owner_loads(&g, "a").await;

    answered_by(&g, chat(&gw, "jobs").await, "jobs", "a").await;
    assert!(g.stops().is_empty(), "the owner's model was not evicted");
    assert_eq!(g.runs(), ["x", "a"], "the primary was not started");
}

/// Nothing usable: the alias fallback answers with reason `background`; with
/// none, `503 gpu_hold` logged `gpu_deferred`, saying what is in the way. No
/// eviction either way.
#[tokio::test]
async fn a_background_alias_with_nothing_usable_goes_to_its_fallback_or_defers() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("x", 20 * GIB).await;
    g.model("p", 12 * GIB).await;
    g.model("a", 4 * GIB).await;
    g.cloud("cloud", None).await;
    g.candidate(alias("jobs", &["p", "a"], true, Some("cloud"), &[]))
        .await;
    g.candidate(alias("jobs-local", &["p", "a"], true, None, &[]))
        .await;
    owner_loads(&g, "x").await;

    answered_by_fallback(&g, chat(&gw, "jobs").await, "jobs", "cloud", "background").await;

    let resp = chat(&gw, "jobs-local").await;
    assert_eq!(resp.status(), 503);
    assert_eq!(header(&resp, "x-lmgw-candidate"), None);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "gpu_hold", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("deferred: GPU in use by"), "{msg}");
    assert!(!msg.contains("  "), "one sentence, no stray spaces: {msg}");
    let row = newest_log(&g, "jobs-local").await;
    assert_eq!(row.error_kind.as_deref(), Some("gpu_deferred"), "{row:?}");

    assert!(g.stops().is_empty());
    assert_eq!(g.runs(), ["x"]);
}

// ---------------------------------------------------------------------------
// §7 item 11: the hold
// ---------------------------------------------------------------------------

/// Both alias kinds go straight to the alias fallback under the hold — a
/// candidate's own row fallback is never consulted — and `none` is the
/// hold's own refusal, logged `gpu_hold`, not a deferral.
#[tokio::test]
async fn the_hold_answers_both_alias_kinds_with_the_alias_fallback_never_a_row_fallback() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.cloud("cloud", None).await;
    g.cloud("rowcloud", None).await;
    let mut p = row("p", None);
    p.hold_fallback_mode = HoldFallbackMode::Alias;
    p.hold_fallback = Some("rowcloud".into());
    g.row(p, 8 * GIB).await;
    g.candidate(alias("writer", &["p"], false, Some("cloud"), &[]))
        .await;
    g.candidate(alias("jobs", &["p"], true, Some("cloud"), &[]))
        .await;
    g.candidate(alias("strict", &["p"], false, None, &[])).await;
    hold_on(&g).await;

    answered_by_fallback(&g, chat(&gw, "writer").await, "writer", "cloud", "hold").await;
    answered_by_fallback(&g, chat(&gw, "jobs").await, "jobs", "cloud", "hold").await;

    let resp = chat(&gw, "strict").await;
    assert_eq!(resp.status(), 503);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "gpu_hold");
    let row = newest_log(&g, "strict").await;
    assert_eq!(row.error_kind.as_deref(), Some("gpu_hold"), "{row:?}");

    assert!(g.runs().is_empty(), "nothing local starts under the hold");
}

// ---------------------------------------------------------------------------
// §7 item 12: capabilities, the routing half
// ---------------------------------------------------------------------------

/// A request that uses a facet the alias does not enable is a `400` naming
/// it, before anything starts — in both dialects.
#[tokio::test]
async fn a_request_using_a_facet_the_alias_does_not_enable_is_a_400() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.row(row("p", Some(caps(true))), 8 * GIB).await;
    g.candidate(alias("writer", &["p"], false, None, &[])).await;

    let resp = chat_body(
        &gw,
        json!({"model": "writer", "messages": [{"role": "user", "content": [
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
        ]}]}),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "unsupported", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("'writer' does not enable Vision"), "{msg}");

    let resp = gw
        .client()
        .post(format!("{gw}/v1/messages"))
        .json(&json!({"model": "writer", "max_tokens": 16,
                      "messages": [{"role": "user", "content": "hi"}],
                      "tools": [{"name": "f", "input_schema": {"type": "object"}}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Tool calls"),
        "{v}"
    );

    assert!(g.runs().is_empty(), "refused before anything started");
}

/// A candidate whose row no longer supports an enabled facet is skipped: a
/// background alias goes on to its loaded alternate, and an owner alias
/// with its primary skipped and nothing loaded answers from its fallback
/// (reason `unavailable`, §12 entry 70) — or, with none, is the named
/// `candidate_unavailable`. Nothing is started either way.
#[tokio::test]
async fn a_candidate_that_lost_an_enabled_facet_is_skipped() {
    let (g, gw) = world(24 * GIB, 2).await;
    // The alias enables vision; the primary's row says it has none now.
    g.row(row("p", Some(caps(false))), 8 * GIB).await;
    g.row(row("a", Some(caps(true))), 4 * GIB).await;
    g.cloud("cloud", Some(caps(true))).await;
    g.candidate(alias("jobs", &["p", "a"], true, None, &["vision"]))
        .await;
    g.candidate(alias(
        "writer",
        &["p", "a"],
        false,
        Some("cloud"),
        &["vision"],
    ))
    .await;
    g.candidate(alias("writer-local", &["p", "a"], false, None, &["vision"]))
        .await;

    answered_by_fallback(
        &g,
        chat(&gw, "writer").await,
        "writer",
        "cloud",
        "unavailable",
    )
    .await;
    let resp = chat(&gw, "writer-local").await;
    assert_eq!(resp.status(), 503);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "candidate_unavailable", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("its primary 'p' lacks vision"), "{msg}");
    let row = newest_log(&g, "writer-local").await;
    assert_eq!(
        row.error_kind.as_deref(),
        Some("candidate_unavailable"),
        "{row:?}"
    );
    assert!(g.runs().is_empty(), "the skipped primary was not started");

    owner_loads(&g, "a").await;
    // The primary would fit the free VRAM, and is still not started.
    answered_by(&g, chat(&gw, "jobs").await, "jobs", "a").await;
    answered_by(&g, chat(&gw, "writer").await, "writer", "a").await;
    assert_eq!(g.runs(), ["a"]);
}

/// A disabled or deleted primary is the same drift as a lost facet: the
/// owner alias's fallback answers, reason `unavailable`.
#[tokio::test]
async fn an_owner_alias_whose_primary_is_disabled_answers_from_its_fallback() {
    let (g, gw) = world(24 * GIB, 2).await;
    let mut p = row("p", None);
    p.enabled = false;
    g.row(p, 8 * GIB).await;
    g.cloud("cloud", None).await;
    g.candidate(alias("writer", &["p"], false, Some("cloud"), &[]))
        .await;
    g.candidate(alias("gone", &["nowhere"], false, Some("cloud"), &[]))
        .await;

    answered_by_fallback(
        &g,
        chat(&gw, "writer").await,
        "writer",
        "cloud",
        "unavailable",
    )
    .await;
    answered_by_fallback(&g, chat(&gw, "gone").await, "gone", "cloud", "unavailable").await;
    assert!(g.runs().is_empty());
}

/// A fallback that lacks a facet the alias enables (Vision) is used all the
/// same (changed 2026-10-06, the owner's ruling: a configured fallback is
/// always used; until then it counted as none, §4.6): a guest whose primary
/// is blocked is answered by it, and under the hold too. What it cannot
/// take goes to it degraded — an image as its placeholder — and the request
/// row says so (`request_logs.degraded`); a request without images loses
/// nothing and is not marked.
#[tokio::test]
async fn a_fallback_lacking_an_enabled_facet_is_used_and_degrades() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("x", 20 * GIB).await;
    g.row(row("p", Some(caps(true))), 8 * GIB).await;
    g.cloud("blind", Some(caps(false))).await;
    g.candidate(alias("writer", &["p"], false, Some("blind"), &["vision"]))
        .await;
    g.candidate(alias("jobs", &["p"], true, Some("blind"), &["vision"]))
        .await;
    owner_loads(&g, "x").await;
    let with_image = |model: &str| {
        json!({"model": model, "messages": [{"role": "user", "content": [
            {"type": "text", "text": "what is this"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
        ]}]})
    };
    let marker = "fallback 'blind' lacks vision: 1 image sent as a placeholder";

    answered_by_fallback(&g, chat(&gw, "jobs").await, "jobs", "blind", "background").await;
    assert_eq!(newest_log(&g, "jobs").await.degraded, None);
    let resp = chat_body(&gw, with_image("jobs")).await;
    assert_eq!(header(&resp, "x-lmgw-images-omitted"), Some("1"));
    answered_by_fallback(&g, resp, "jobs", "blind", "background").await;
    assert_eq!(
        newest_log(&g, "jobs").await.degraded.as_deref(),
        Some(marker)
    );

    hold_on(&g).await;

    let resp = chat_body(&gw, with_image("writer")).await;
    answered_by_fallback(&g, resp, "writer", "blind", "hold").await;
    let row = newest_log(&g, "writer").await;
    assert_eq!(row.degraded.as_deref(), Some(marker), "{row:?}");
}

// ---------------------------------------------------------------------------
// §7 item 13, last bullet: the outside-VRAM verdict
// ---------------------------------------------------------------------------

/// A non-background alias answers §4.7's outside-VRAM verdict with its own
/// fallback — never its primary's row fallback — at once, starting nothing.
#[tokio::test]
async fn a_non_background_alias_uses_its_alias_fallback_on_the_outside_vram_verdict() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.cloud("cloud", None).await;
    g.cloud("rowcloud", None).await;
    let mut p = row("p", None);
    p.hold_fallback_mode = HoldFallbackMode::Alias;
    p.hold_fallback = Some("rowcloud".into());
    g.row(p, 8 * GIB).await;
    g.candidate(alias("writer", &["p"], false, Some("cloud"), &[]))
        .await;
    {
        let mut w = g.world();
        w.attribution = true;
        w.outside = 20 * GIB;
    }

    let waits = g.state.vram.waits_begun();
    let resp = chat(&gw, "writer").await;
    assert_eq!(g.state.vram.waits_begun(), waits, "answered at once");
    answered_by_fallback(&g, resp, "writer", "cloud", "external_vram").await;
    assert!(g.runs().is_empty());
}

// ---------------------------------------------------------------------------
// §12 entry 45: the send-time re-pick
// ---------------------------------------------------------------------------

/// llama-server refuses the guest's prompt on the primary before any work:
/// held back, and the loaded alternate answers. The client never sees the
/// primary's refusal.
#[tokio::test]
async fn a_guest_over_a_candidates_context_is_repicked_onto_a_loaded_alternate() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("p", 8 * GIB).await;
    g.model("a", 8 * GIB).await;
    g.candidate(alias("jobs", &["p", "a"], true, None, &[]))
        .await;
    owner_loads(&g, "p").await;
    owner_loads(&g, "a").await;
    g.world().refuse_context.insert("p".into());

    answered_by(&g, chat(&gw, "jobs").await, "jobs", "a").await;
    assert_eq!(g.world().chats, ["a"]);
    assert!(g.stops().is_empty());
}

/// A guest's request needs a climb of the owner's ladder, which it may not
/// make (§9): denied, and the loaded alternate answers — the ladder stays
/// on its base rung.
#[tokio::test]
async fn a_guest_denied_a_climb_of_the_owners_ladder_is_repicked_onto_a_loaded_alternate() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.ladder("p", 4 * GIB, &[(8 * GIB, 4096)]).await;
    g.model("a", 4 * GIB).await;
    g.candidate(alias("jobs", &["p", "a"], true, None, &[]))
        .await;
    owner_loads(&g, "p").await;
    owner_loads(&g, "a").await;
    // 100 prompt tokens + 16 of output do not fit the base rung's 64.
    g.world().prompt_tokens = 100;

    answered_by(&g, chat(&gw, "jobs").await, "jobs", "a").await;
    assert_eq!(g.runs(), ["p", "a"], "no climb was started");
    assert!(g.stops().is_empty());
    assert_eq!(view(&g, "p").unwrap().owner, Origin::Owner);
}

/// Every loaded candidate refuses the guest's prompt: the alias fallback,
/// reason `background`; with none, the deferral names the last one tried.
#[tokio::test]
async fn a_guest_with_every_loaded_candidate_excluded_goes_to_its_fallback_or_defers() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("p", 8 * GIB).await;
    g.model("a", 8 * GIB).await;
    g.cloud("cloud", None).await;
    g.candidate(alias("jobs", &["p", "a"], true, Some("cloud"), &[]))
        .await;
    g.candidate(alias("jobs-local", &["p", "a"], true, None, &[]))
        .await;
    owner_loads(&g, "p").await;
    owner_loads(&g, "a").await;
    {
        let mut w = g.world();
        w.refuse_context.insert("p".into());
        w.refuse_context.insert("a".into());
    }

    answered_by_fallback(&g, chat(&gw, "jobs").await, "jobs", "cloud", "background").await;

    let resp = chat(&gw, "jobs-local").await;
    assert_eq!(resp.status(), 503);
    assert_eq!(header(&resp, "x-lmgw-candidate"), None, "nothing answered");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "gpu_hold");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("'a' cannot hold this request"), "{msg}");
    let row = newest_log(&g, "jobs-local").await;
    assert_eq!(row.error_kind.as_deref(), Some("gpu_deferred"), "{row:?}");

    assert!(g.world().chats.is_empty(), "no candidate answered");
    assert_eq!(g.runs(), ["p", "a"]);
}

/// An alternate's container dies under an owner request: the alias never
/// restarts an alternate (`candidate_lost`), so the walk picks again and
/// ends at the primary, loaded through normal admission.
#[tokio::test]
async fn a_lost_alternate_is_repicked_and_an_owner_alias_ends_at_its_primary() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("p", 8 * GIB).await;
    g.model("a", 8 * GIB).await;
    g.candidate(alias("writer", &["p", "a"], false, None, &[]))
        .await;
    let port = owner_loads(&g, "a").await;
    g.kill(port).await;

    answered_by(&g, chat(&gw, "writer").await, "writer", "p").await;
    assert_eq!(g.runs(), ["a", "p"], "the alternate was never restarted");
}

// ---------------------------------------------------------------------------
// Names that are not candidate aliases (§7 item 14)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plain_names_carry_no_candidate_header() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("p", 8 * GIB).await;
    g.cloud("cloud", None).await;
    g.candidate(alias("writer", &["p"], false, None, &[])).await;

    let resp = chat(&gw, "p").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-candidate"), None);
    let resp = chat(&gw, "cloud").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-candidate"), None);
}

/// A caller pinned to one model's tokenizer or vectors cannot name a
/// candidate alias.
#[tokio::test]
async fn a_pinned_caller_is_refused_a_candidate_alias() {
    let (g, _gw) = world(24 * GIB, 2).await;
    g.model("p", 8 * GIB).await;
    g.candidate(alias("writer", &["p"], false, None, &[])).await;

    let err = lmgw_core::gate::open_pinned(
        &g.state,
        "writer",
        lmgw_core::gate::RouteCheck::Text("/v1/count_tokens"),
    )
    .await
    .unwrap_err();
    assert_eq!(err.error.kind(), "unsupported", "{}", err.error);
    assert!(g.runs().is_empty());
}

// ---------------------------------------------------------------------------
// §12 entries 62, 74: a primary that cannot be brought up
// ---------------------------------------------------------------------------

async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// A direct request starts `model` and its `podman run` waits at the world's
/// run gate; returns once the registry shows the start in flight.
async fn start_held_in_flight(g: &Gpu, model: &str) -> tokio::task::JoinHandle<bool> {
    let direct = tokio::spawn({
        let (state, route, model) = (g.state.clone(), g.route(model), model.to_string());
        async move { vram::admit(&state, &route, &model).await.is_ok() }
    });
    until("the direct start is in flight", || {
        view(g, model).is_some_and(|v| v.state == RuntimeState::Starting)
    })
    .await;
    direct
}

/// A request sent now, answered in the background.
fn chat_later(gw: &Gw, model: &str) -> tokio::task::JoinHandle<reqwest::Response> {
    let (gw, model) = (gw.clone(), model.to_string());
    tokio::spawn(async move { chat(&gw, &model).await })
}

async fn answer(handle: tokio::task::JoinHandle<reqwest::Response>) -> reqwest::Response {
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("the request was answered, not left waiting on a second start")
        .unwrap()
}

/// The primary's start fails while owner-alias requests have joined it: the
/// walk never starts it a second time. A loaded alternate answers; with
/// none, the start's own error is the answer, naming the primary.
#[tokio::test]
async fn an_owner_alias_never_restarts_a_primary_whose_joined_start_failed() {
    let (g, gw) = world(24 * GIB, 4).await;
    g.model("p", 8 * GIB).await;
    g.model("a", 4 * GIB).await;
    g.candidate(alias("writer", &["p"], false, None, &[])).await;
    g.candidate(alias("writer-a", &["p", "a"], false, None, &[]))
        .await;
    owner_loads(&g, "a").await;
    g.world().fail_run.insert("p".into());
    let open = g.gate_runs();

    let direct = start_held_in_flight(&g, "p").await;
    let alone = chat_later(&gw, "writer");
    let with_alternate = chat_later(&gw, "writer-a");
    // Both walks reach the primary's start and park on it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    open.send(true).unwrap();

    assert!(!direct.await.unwrap(), "the direct start failed");
    let resp = answer(alone).await;
    assert_eq!(resp.status(), 502, "{:?}", resp.headers());
    assert_eq!(header(&resp, "x-lmgw-candidate"), Some("p"));
    answered_by(&g, answer(with_alternate).await, "writer-a", "a").await;
    assert_eq!(g.runs(), ["a", "p"], "the failed primary was tried once");
}

/// A stop lands on the primary's start while an owner-alias request has
/// joined it: the model somebody just stopped is not brought back by the
/// alias — the request gets the stop's error, as a direct one would.
#[tokio::test]
async fn an_owner_alias_never_brings_back_a_primary_stopped_while_it_started() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("p", 8 * GIB).await;
    g.candidate(alias("writer", &["p"], false, None, &[])).await;
    let _open = g.gate_runs();

    let _direct = start_held_in_flight(&g, "p").await;
    let joined = chat_later(&gw, "writer");
    tokio::time::sleep(Duration::from_millis(300)).await;
    g.state
        .runtime()
        .stop(lmgw_core::runtime::Class::Chat, "p", false)
        .await
        .unwrap();

    let resp = answer(joined).await;
    assert_eq!(resp.status(), 502, "{:?}", resp.headers());
    assert_eq!(header(&resp, "x-lmgw-candidate"), Some("p"));
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("stopped while it was starting"), "{msg}");
    assert_eq!(g.runs(), ["p"], "no second start");
    assert!(view(&g, "p").is_none(), "it stays down");
}

/// A guest whose primary cannot be brought up goes on as the owner's order
/// says — "load primary if possible, if any candidate is loaded use that,
/// otherwise use fallback" (§12 entry 74): a start that fails and a primary
/// no card could hold both lead to the loaded alternate, else the fallback
/// (`background`); with no fallback the primary's own error is the answer.
#[tokio::test]
async fn a_guest_whose_primary_cannot_be_brought_up_goes_on_to_alternates_and_the_fallback() {
    let (g, gw) = world(24 * GIB, 8).await;
    g.model("p", 8 * GIB).await;
    g.model("big", 30 * GIB).await;
    g.model("a", 4 * GIB).await;
    g.cloud("cloud", None).await;
    g.candidate(alias("jobs", &["p", "a"], true, None, &[]))
        .await;
    g.candidate(alias("jobs-cloud", &["p"], true, Some("cloud"), &[]))
        .await;
    g.candidate(alias("jobs-local", &["p"], true, None, &[]))
        .await;
    g.candidate(alias("big-jobs", &["big", "a"], true, None, &[]))
        .await;
    g.candidate(alias("big-local", &["big"], true, None, &[]))
        .await;
    owner_loads(&g, "a").await;
    g.world().fail_run.insert("p".into());

    answered_by(&g, chat(&gw, "jobs").await, "jobs", "a").await;
    answered_by_fallback(
        &g,
        chat(&gw, "jobs-cloud").await,
        "jobs-cloud",
        "cloud",
        "background",
    )
    .await;
    let resp = chat(&gw, "jobs-local").await;
    assert_eq!(resp.status(), 502, "the start's error, not a deferral");
    assert_eq!(header(&resp, "x-lmgw-candidate"), Some("p"));
    let row = newest_log(&g, "jobs-local").await;
    assert_ne!(row.error_kind.as_deref(), Some("gpu_deferred"), "{row:?}");

    answered_by(&g, chat(&gw, "big-jobs").await, "big-jobs", "a").await;
    let resp = chat(&gw, "big-local").await;
    assert_eq!(header(&resp, "x-lmgw-candidate"), Some("big"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "vram_too_large", "{v}");

    assert_eq!(
        g.runs(),
        ["a", "p", "p", "p"],
        "one try per request, never big"
    );
    assert!(g.stops().is_empty());
}

// ---------------------------------------------------------------------------
// §12 entry 86: the alias a request started on
// ---------------------------------------------------------------------------

/// A request that started on a candidate alias keeps that alias's fallback
/// when the alias is deleted while it is in flight: a climb under the GPU
/// hold answers with the alias fallback — never the candidate's own row
/// fallback, which the alias never uses (§4.1).
#[tokio::test]
async fn a_request_keeps_its_alias_fallback_when_the_alias_goes_away_mid_flight() {
    let (g, _gw) = world(24 * GIB, 3).await;
    g.cloud("cloud", None).await;
    g.cloud("rowcloud", None).await;
    g.file("p-2.gguf", 8 * GIB);
    let mut p = row("p", None);
    p.params = lmgw_core::config::LlamaParams {
        ctx_size: Some(64),
        parallel: Some(1),
        n_predict: Some(16),
        ..Default::default()
    };
    p.ladder = vec![lmgw_core::ladder::Rung {
        gguf_path: "p-2.gguf".into(),
        ctx_size: 4096,
    }];
    p.hold_fallback_mode = HoldFallbackMode::Alias;
    p.hold_fallback = Some("rowcloud".into());
    g.row(p, 4 * GIB).await;
    g.candidate(alias("writer", &["p"], false, Some("cloud"), &[]))
        .await;

    let opened = lmgw_core::gate::open(
        &g.state,
        "writer",
        lmgw_core::gate::RouteCheck::Text("/v1/chat/completions"),
    )
    .await
    .unwrap();
    let hold = opened.hold.expect("the alias's primary is local");
    let id = g.state.snapshot().candidate_aliases["writer"].id;
    store::delete_candidate_alias(&g.state.db, id)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    hold_on(&g).await;

    match vram::climb(&g.state, &hold, 1, "a test").await.unwrap() {
        vram::Climbed::Fallback { alias, reason, .. } => {
            assert_eq!(alias, "cloud", "the alias's fallback, not the row's");
            assert_eq!(reason, lmgw_core::gate::FallbackReason::Hold);
        }
        other => panic!("expected the alias fallback, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// §12 entry 87: the pick follows the files it was derived from
// ---------------------------------------------------------------------------

/// The pick is derived again when a candidate's weights file changes on
/// disk, not only on a config write: a primary whose GGUF could not be read
/// is not kept out of `routable` once it can be, a readable one is served
/// from the cache, and one replaced by an unreadable file is not kept in.
#[tokio::test]
async fn the_pick_follows_a_candidates_weights_file_without_a_config_write() {
    use lmgw_core::candidates::derive::cached_pick;

    let (g, _gw) = world(24 * GIB, 2).await;
    // The sparse weights are no GGUF: nothing can be read from them yet.
    g.model("p", 4 * GIB).await;
    g.candidate(alias("writer", &["p"], false, None, &["structured_output"]))
        .await;
    let snap = g.state.snapshot();
    let ca = snap.candidate_alias("writer").unwrap().clone();

    assert!(
        cached_pick(&g.state, &snap, &ca).await.routable.is_empty(),
        "unreadable weights support nothing"
    );

    g.write_gguf("p.gguf");
    assert_eq!(cached_pick(&g.state, &snap, &ca).await.routable, ["p"]);
    let reads = g.state.gguf_cache.reads();
    assert_eq!(cached_pick(&g.state, &snap, &ca).await.routable, ["p"]);
    assert_eq!(
        g.state.gguf_cache.reads(),
        reads,
        "the second pick is cached"
    );

    std::fs::write(g.models_dir().join("p.gguf"), b"not a gguf").unwrap();
    assert!(
        cached_pick(&g.state, &snap, &ca).await.routable.is_empty(),
        "the replaced file is read again"
    );
    assert!(
        Arc::ptr_eq(&snap, &g.state.snapshot()),
        "no config write happened"
    );
}

// ---------------------------------------------------------------------------
// §12 entry 88: a re-pick leaves the old candidate's pool first
// ---------------------------------------------------------------------------

/// A guest's send on a guarded unified-KV candidate is refused by
/// llama-server before any work: its pool reservation is back before the
/// re-pick, not held while the re-pick waits on the next candidate's start
/// — other requests on that pool do not queue behind a request that left.
#[tokio::test]
async fn a_repick_releases_the_old_candidates_pool_reservation_first() {
    let (g, gw) = world(24 * GIB, 3).await;
    let mut p = row("p", None);
    p.params = lmgw_core::config::LlamaParams {
        ctx_size: Some(4096),
        parallel: Some(2),
        kv_unified: Some(true),
        n_predict: Some(16),
        ..Default::default()
    };
    g.row(p, 4 * GIB).await;
    g.model("a", 4 * GIB).await;
    g.candidate(alias("jobs", &["p", "a"], true, None, &[]))
        .await;
    owner_loads(&g, "p").await;
    {
        let mut w = g.world();
        w.prompt_tokens = 100;
        w.refuse_context.insert("p".into());
    }
    let open = g.gate_runs();
    let _direct = start_held_in_flight(&g, "a").await;

    let guest = chat_later(&gw, "jobs");
    // p refuses the guest at once, and the re-pick parks on a's start.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let pool = g.state.kv_pools.view().into_iter().find(|v| v.model == "p");
    assert!(
        pool.as_ref()
            .is_none_or(|v| v.reserved_tokens == 0 && v.releasing == 0),
        "the reservation is back while the re-pick waits: {pool:?}"
    );

    open.send(true).unwrap();
    answered_by(&g, answer(guest).await, "jobs", "a").await;
}

// ---------------------------------------------------------------------------
// §12 entry 73: the re-pick's context triggers
// ---------------------------------------------------------------------------

/// A guest over a guarded unified-KV pool's per-request limit is found out
/// by the fit, before anything is sent (`TurnLease::skip`): picked again
/// onto the loaded alternate, the guarded candidate never sees the request.
/// An owner request over the same limit gets the model's `400`, as a direct
/// request would.
#[tokio::test]
async fn a_guest_over_a_guarded_pools_limit_is_repicked_before_the_send() {
    let (g, gw) = world(24 * GIB, 3).await;
    let mut p = row("p", None);
    p.params = lmgw_core::config::LlamaParams {
        ctx_size: Some(64),
        parallel: Some(2),
        kv_unified: Some(true),
        n_predict: Some(16),
        ..Default::default()
    };
    g.row(p, 4 * GIB).await;
    g.model("a", 4 * GIB).await;
    g.candidate(alias("jobs", &["p", "a"], true, None, &[]))
        .await;
    g.candidate(alias("writer", &["p", "a"], false, None, &[]))
        .await;
    owner_loads(&g, "p").await;
    owner_loads(&g, "a").await;
    // 100 prompt tokens + 16 of output do not fit a 64-token request.
    g.world().prompt_tokens = 100;

    answered_by(&g, chat(&gw, "jobs").await, "jobs", "a").await;
    assert_eq!(g.world().chats, ["a"], "p never saw the guest's request");

    let resp = chat(&gw, "writer").await;
    assert_eq!(resp.status(), 400);
    assert_eq!(header(&resp, "x-lmgw-candidate"), Some("p"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "context_length_exceeded", "{v}");
    assert_eq!(g.runs(), ["p", "a"]);
}

/// A guest whose prompt does not fit even a ladder candidate's top rung is
/// picked again onto the loaded alternate; nothing is climbed. An owner
/// request gets the ladder's `400`, naming its top rung.
#[tokio::test]
async fn a_guest_above_a_ladders_top_rung_is_repicked_onto_a_loaded_alternate() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.ladder("p", 4 * GIB, &[(8 * GIB, 4096)]).await;
    g.model("a", 4 * GIB).await;
    g.candidate(alias("jobs", &["p", "a"], true, None, &[]))
        .await;
    g.candidate(alias("writer", &["p", "a"], false, None, &[]))
        .await;
    owner_loads(&g, "p").await;
    owner_loads(&g, "a").await;
    // 10 000 prompt tokens do not fit the top rung's 4096.
    g.world().prompt_tokens = 10_000;

    answered_by(&g, chat(&gw, "jobs").await, "jobs", "a").await;
    assert_eq!(g.runs(), ["p", "a"], "no rung was started");

    let resp = chat(&gw, "writer").await;
    assert_eq!(resp.status(), 400);
    assert_eq!(header(&resp, "x-lmgw-candidate"), Some("p"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "context_length_exceeded", "{v}");
    assert_eq!(g.runs(), ["p", "a"]);
    assert!(g.stops().is_empty());
}

// ---------------------------------------------------------------------------
// §7 item 9, end to end
// ---------------------------------------------------------------------------

/// The owner waits for room a busy background model holds (§4.5): the model
/// drains — a new background request is not given it and goes to the alias
/// fallback, since every resident drains while the owner waits (§12 entry
/// 47) — the guest's request in flight completes, the idle guest model is
/// evicted, and the owner's model starts and answers.
#[tokio::test]
async fn an_owner_waiting_on_a_busy_guest_model_drains_it_and_then_starts_his() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("b", 16 * GIB).await;
    g.model("x", 16 * GIB).await;
    g.cloud("cloud", None).await;
    g.candidate(alias("jobs", &["b"], true, Some("cloud"), &[]))
        .await;
    // The guest's request in flight on the primary it started.
    let in_flight = match vram::start_background(&g.state, &g.route("b"), "jobs")
        .await
        .unwrap()
    {
        vram::BackgroundStart::Started(hold) => hold,
        vram::BackgroundStart::Blocked(why) => panic!("the guest could not start: {why}"),
    };
    assert_eq!(view(&g, "b").unwrap().owner, Origin::Background);

    let owner = chat_later(&gw, "x");
    let runtime = g.state.runtime();
    until("the owner waits for room", || runtime.draining_for_owner()).await;
    assert!(view(&g, "b").unwrap().draining_for_owner);

    answered_by_fallback(&g, chat(&gw, "jobs").await, "jobs", "cloud", "background").await;
    assert!(g.stops().is_empty(), "the busy guest model was not stopped");

    drop(in_flight);
    let resp = answer(owner).await;
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "ok from x");
    assert_eq!(g.stops(), ["b"]);
    assert_eq!(g.runs(), ["b", "x"]);
    assert!(!g.state.runtime().draining_for_owner(), "the mark cleared");
}
