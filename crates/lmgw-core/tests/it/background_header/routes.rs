//! The guest branch on the routes `classes.rs` does not drive (per-request
//! background design §7): audio speech, the voice list's engine probe, the
//! token counters and `/v1/systemone` all reach the gate with their
//! request's origin, so on a card the owner's model fills each defers rather
//! than evict it. A row on the CPU is admitted as today (§6), and a bad
//! header value on `/v1/systemone` is llama.cpp's `400` (§3).

use lmgw_core::config::AuxKind;
use lmgw_core::runtime::registry::Origin;
use serde_json::{json, Value};

use super::*;
use crate::support::gpu_world::GIB;

/// The card, full with the owner's idle chat model `x` (20 of 24 GiB).
async fn full_card() -> (Gpu, Gw) {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 20 * GIB).await;
    owner_loads(&g, "x").await;
    (g, gw)
}

async fn post(gw: &Gw, path: &str, body: Value) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}{path}"))
        .header(BACKGROUND, "on")
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// `503 gpu_deferred` in the route's own shape — OpenAI's `code`, or
/// llama.cpp's `lmgw_code` — naming `named`, and nothing evicted or started.
async fn deferred_here(g: &Gpu, resp: reqwest::Response, named: &str) {
    assert_eq!(resp.status(), 503, "{:?}", resp.headers());
    let v: Value = resp.json().await.unwrap();
    let code = v["error"]["lmgw_code"]
        .as_str()
        .or(v["error"]["code"].as_str());
    assert_eq!(code, Some("gpu_deferred"), "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains(&format!("'{named}' deferred: ")), "{msg}");
    assert!(g.stops().is_empty(), "the owner's model was not evicted");
    assert_eq!(g.runs(), ["x"], "nothing started");
}

#[tokio::test]
async fn speech_and_the_engine_voice_probe_defer() {
    let (g, gw) = full_card().await;
    g.audio("tts", "tts", 6 * GIB).await;

    let resp = post(
        &gw,
        "/v1/audio/speech",
        json!({"model": "audio/tts", "input": "Hallo.", "voice": "alba"}),
    )
    .await;
    deferred_here(&g, resp, "audio/tts").await;
    let row = newest_log(&g, "audio/tts").await;
    assert_eq!(row.error_kind.as_deref(), Some("gpu_deferred"), "{row:?}");

    let resp = gw
        .client()
        .get(format!("{gw}/v1/audio/voices?model=audio/tts&probe=engine"))
        .header(BACKGROUND, "on")
        .send()
        .await
        .unwrap();
    deferred_here(&g, resp, "audio/tts").await;
    assert!(g.world().speeches.is_empty());
}

#[tokio::test]
async fn the_token_counters_defer_in_their_own_shapes() {
    let (g, gw) = full_card().await;
    g.model("m", 12 * GIB).await;

    let resp = post(
        &gw,
        "/v1/count_tokens",
        json!({"model": "m", "input": "hi"}),
    )
    .await;
    deferred_here(&g, resp, "m").await;
    let resp = post(&gw, "/tokenize", json!({"model": "m", "content": "hi"})).await;
    deferred_here(&g, resp, "m").await;
    assert!(g.world().tokenized.is_empty(), "nothing was counted");
}

#[tokio::test]
async fn systemone_defers_and_refuses_a_bad_value_in_llama_cpp_s_shape() {
    let (g, gw) = full_card().await;
    g.aux("dec", AuxKind::Decision, 6 * GIB).await;
    let body = json!({"model": "decide/dec", "state": "hi",
                      "questions": {"q": {"type": "noul", "instructions": "Is it?"}}});

    let resp = post(&gw, "/v1/systemone", body.clone()).await;
    deferred_here(&g, resp, "decide/dec").await;

    let resp = gw
        .client()
        .post(format!("{gw}/v1/systemone"))
        .header(BACKGROUND, "maybe")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], 400, "{v}");
    assert_eq!(v["error"]["lmgw_code"], "bad_request", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("x-lmgw-background: expected 'on' or 'off', got 'maybe'"),
        "{v}"
    );
    assert_eq!(g.runs(), ["x"]);
}

/// An embedder on the CPU takes no VRAM, so there is no owner to disturb:
/// admitted as any request is, the header changing nothing.
#[tokio::test]
async fn a_row_on_the_cpu_is_admitted_as_today() {
    let (g, gw) = full_card().await;
    g.aux("cpu-emb", AuxKind::Embed, 6 * GIB).await;
    sqlx::query("UPDATE aux_models SET backend = 'cpu' WHERE model_id = 'cpu-emb'")
        .execute(&g.state.db)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();

    let resp = post(
        &gw,
        "/v1/embeddings",
        json!({"model": "embed/cpu-emb", "input": "hi"}),
    )
    .await;
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    assert!(g.stops().is_empty());
    assert_eq!(g.runs(), ["x", "cpu-emb"]);
    assert_eq!(view(&g, "cpu-emb").unwrap().owner, Origin::Owner);
}
