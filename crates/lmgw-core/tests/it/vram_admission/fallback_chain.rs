//! The chain (openai egress design §14.3), end to end: a fallback is served
//! as if the client had named it, so a local fallback that cannot answer
//! either goes on to its own fallback, each hop with its reason in
//! `x-lmgw-fallback-chain` (§14.5). Over HTTP on `support/gpu_world.rs`,
//! whose containers answer "ok from <model>" and whose cloud aliases answer
//! "from the cloud"; the CPU audio hop on this module's own fixture.

use lmgw_core::runtime::registry::Origin;
use lmgw_core::store::NewCandidateAlias;

use super::audio_cpu::add_cpu_model;
use super::audio_residency::add_audio_model;
use super::*;
use crate::support::fallbacks::set_fallback;
use crate::support::gpu_world::Gpu;

async fn world(total: u64, containers: usize) -> (Gpu, Gw) {
    let g = Gpu::new(total, containers, 2).await;
    let gw = common::serve(g.state.clone()).await;
    (g, gw)
}

/// A chat request for `model`; background traffic when `guest`.
async fn ask(gw: &Gw, model: &str, guest: bool) -> reqwest::Response {
    let mut req = gw
        .client()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}));
    if guest {
        req = req.header("x-lmgw-background", "on");
    }
    req.send().await.unwrap()
}

fn h(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

/// A 200 for `requested` that the last hop of `chain` answered with
/// `content`: `x-lmgw-fallback` is that hop, `x-lmgw-fallback-reason` its
/// reason, `x-lmgw-fallback-chain` the whole list, and the body still names
/// the requested model.
async fn answered_along(resp: reqwest::Response, requested: &str, chain: &str, content: &str) {
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let last = chain.rsplit(", ").next().unwrap();
    let (name, reason) = last.split_once(";reason=").unwrap();
    assert_eq!(
        h(&resp, "x-lmgw-fallback").as_deref(),
        Some(name.trim_matches('"'))
    );
    assert_eq!(h(&resp, "x-lmgw-fallback-reason").as_deref(), Some(reason));
    assert_eq!(h(&resp, "x-lmgw-fallback-chain").as_deref(), Some(chain));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], content, "{v}");
    assert_eq!(v["model"], requested, "the body echoes the requested name");
}

/// A `503 gpu_hold` whose message has every one of `parts`.
async fn held(resp: reqwest::Response, parts: &[&str]) {
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    for p in parts {
        assert!(msg.contains(p), "{p:?} missing from: {msg}");
    }
}

/// Under the hold a local fallback is held too: the walk goes on to its own
/// fallback, a cloud one answers, and nothing is started on the way.
#[tokio::test]
async fn a_held_model_goes_through_a_held_local_fallback_to_the_cloud() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("m1", 8 * GIB).await;
    g.model("m2", 8 * GIB).await;
    g.cloud("cloud", None).await;
    set_fallback(&g.state, "local_models", "m1", Some("m2")).await;
    set_fallback(&g.state, "local_models", "m2", Some("cloud")).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();

    answered_along(
        ask(&gw, "m1", false).await,
        "m1",
        r#""m2";reason=hold, "cloud";reason=hold"#,
        "from the cloud",
    )
    .await;
    assert!(g.runs().is_empty(), "{:?}", g.runs());
}

/// A held chain whose last hop has no fallback: the last hop's own refusal,
/// the chain named in it.
#[tokio::test]
async fn a_held_chain_without_a_cloud_end_is_refused_naming_the_chain() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("m1", 8 * GIB).await;
    g.model("m2", 8 * GIB).await;
    set_fallback(&g.state, "local_models", "m1", Some("m2")).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();

    held(ask(&gw, "m1", false).await, &["'m2'", "(tried: m1 → m2)"]).await;
    assert!(g.runs().is_empty(), "{:?}", g.runs());
}

/// A loop the save check refuses, written straight into the rows: the walk
/// meets `m1` again and ends with an error naming the loop — never a loop.
#[tokio::test]
async fn a_loop_written_past_the_save_check_ends_the_walk() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("m1", 8 * GIB).await;
    g.model("m2", 8 * GIB).await;
    set_fallback(&g.state, "local_models", "m1", Some("m2")).await;
    set_fallback(&g.state, "local_models", "m2", Some("m1")).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();

    held(
        ask(&gw, "m1", false).await,
        &["(fallback 'm1' was already tried in this request: m1 → m2 → m1)"],
    )
    .await;
    assert!(g.runs().is_empty(), "{:?}", g.runs());
}

/// VRAM outside lmgw keeps `m1` from loading at all: its local fallback
/// that fits is admitted and answers; one that does not fit either goes on
/// to its own fallback, each hop with `external_vram`.
#[tokio::test]
async fn outside_use_hands_a_request_to_a_local_fallback_that_fits_or_along_to_the_cloud() {
    let (g, gw) = world(24 * GIB, 2).await;
    {
        let mut w = g.world();
        w.attribution = true;
        w.outside = 12 * GIB;
    }
    g.model("m1", 16 * GIB).await;
    g.model("small", 4 * GIB).await;
    g.model("big", 14 * GIB).await;
    g.cloud("cloud", None).await;
    set_fallback(&g.state, "local_models", "m1", Some("small")).await;
    set_fallback(&g.state, "local_models", "big", Some("cloud")).await;

    answered_along(
        ask(&gw, "m1", false).await,
        "m1",
        r#""small";reason=external_vram"#,
        "ok from small",
    )
    .await;
    assert_eq!(g.runs(), ["small"]);

    set_fallback(&g.state, "local_models", "m1", Some("big")).await;
    answered_along(
        ask(&gw, "m1", false).await,
        "m1",
        r#""big";reason=external_vram, "cloud";reason=external_vram"#,
        "from the cloud",
    )
    .await;
    assert_eq!(g.runs(), ["small"], "nothing more started");
}

/// Background traffic along a chain: a local fallback is a guest there too
/// — started into free VRAM as the guest's, or deferred in turn, when its
/// own fallback answers.
#[tokio::test]
async fn a_guest_is_a_guest_on_every_local_hop() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 16 * GIB).await;
    g.model("m", 12 * GIB).await;
    g.model("small", 4 * GIB).await;
    g.model("big", 12 * GIB).await;
    g.cloud("cloud", None).await;
    set_fallback(&g.state, "local_models", "m", Some("small")).await;
    set_fallback(&g.state, "local_models", "big", Some("cloud")).await;
    lmgw_core::vram::admit(&g.state, &g.route("x"), "x")
        .await
        .unwrap()
        .expect("a local model");

    answered_along(
        ask(&gw, "m", true).await,
        "m",
        r#""small";reason=background"#,
        "ok from small",
    )
    .await;
    assert_eq!(g.runs(), ["x", "small"]);
    assert!(g.stops().is_empty(), "the owner's model was not evicted");
    let small = g
        .state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == "small")
        .unwrap();
    assert_eq!(small.owner, Origin::Background, "started as the guest's");

    set_fallback(&g.state, "local_models", "m", Some("big")).await;
    answered_along(
        ask(&gw, "m", true).await,
        "m",
        r#""big";reason=background, "cloud";reason=background"#,
        "from the cloud",
    )
    .await;
    // A guest may stop idle guests' models for room (vram/background.rs),
    // `small` among them; never the owner's.
    assert!(!g.stops().contains(&"x".to_string()), "{:?}", g.stops());
    assert_eq!(g.runs(), ["x", "small"], "nothing more started");
}

/// A candidate alias's fallback is a hop like any other: a local one is
/// held too under the hold and goes on to its own fallback.
#[tokio::test]
async fn a_candidate_aliases_local_fallback_is_a_hop_of_the_chain() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("m1", 8 * GIB).await;
    g.model("m2", 8 * GIB).await;
    g.cloud("cloud", None).await;
    g.candidate(NewCandidateAlias {
        alias: "smart".into(),
        candidates: vec!["m1".into()],
        background: false,
        fallback: Some("m2".into()),
        capabilities_disabled: vec![],
        capabilities_enabled: vec![],
        enabled: true,
        notes: String::new(),
    })
    .await;
    set_fallback(&g.state, "local_models", "m2", Some("cloud")).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();

    answered_along(
        ask(&gw, "smart", false).await,
        "smart",
        r#""m2";reason=hold, "cloud";reason=hold"#,
        "from the cloud",
    )
    .await;
    assert!(g.runs().is_empty(), "{:?}", g.runs());
}

/// A GPU audio row held, its fallback a CPU row of the same task: the hold
/// leaves the CPU row alone, so it answers — started for it, nothing else.
#[tokio::test]
async fn a_held_audio_row_falls_back_to_a_cpu_row() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 0).await;
    add_audio_model(&f, "gpu-tts", GIB, GIB, None).await;
    add_cpu_model(&f, "cpu-tts", GIB).await;
    set_fallback(&f.state, "audio_models", "gpu-tts", Some("audio/cpu-tts")).await;
    engage_hold(&f).await;

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/audio/speech", f.gateway))
        .json(&json!({"model": "audio/gpu-tts", "input": "hello", "voice": "alba"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    assert_eq!(
        h(&resp, "x-lmgw-fallback").as_deref(),
        Some("audio/cpu-tts")
    );
    assert_eq!(h(&resp, "x-lmgw-fallback-reason").as_deref(), Some("hold"));
    assert_eq!(
        h(&resp, "x-lmgw-fallback-chain").as_deref(),
        Some(r#""audio/cpu-tts";reason=hold"#)
    );
    resp.bytes().await.unwrap();
    assert_eq!(f.runs(), ["cpu-tts"]);
}
