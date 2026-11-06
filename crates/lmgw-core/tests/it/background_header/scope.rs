//! `x-lmgw-background` is read on its own routes only (per-request
//! background design §11.12): a path that hands its request's context on
//! in-process — the Chat's device callers, realtime, `/mcp` — is the owner's
//! traffic whatever the client sent (§12). A `HEAD` is read as the `GET`
//! axum runs for it.

use lmgw_core::runtime::registry::Origin;
use serde_json::{json, Value};

use super::*;
use crate::support::gpu_world::GIB;

/// A device's dictation with the header set: the device's context goes on
/// to the transcription, and it is still admitted as the owner's — the
/// owner's idle model evicted for the speech model, as for any dictation —
/// never deferred as a guest.
#[tokio::test]
async fn a_device_s_dictation_with_the_header_is_the_owner_s_traffic() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 20 * GIB).await;
    g.audio("ears", "asr", 6 * GIB).await;
    let mut s = g.state.snapshot().settings.clone();
    s.chat_stt_alias = "audio/ears".into();
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
    owner_loads(&g, "x").await;
    let device = crate::support::device_pair::pair(&g.state, &gw, "phone", json!({})).await;

    let t: Value = device
        .client
        .post(format!("{gw}/chat/api/threads"))
        .json(&json!({"model_alias": "x"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = t["id"].as_i64().unwrap_or_else(|| panic!("a thread: {t}"));
    let resp = device
        .client
        .post(format!("{gw}/chat/api/threads/{tid}/transcribe"))
        .header(BACKGROUND, "on")
        .header("content-type", "audio/wav")
        .body(crate::chat_voice_dictation::recording(1))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let v: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{v}");
    assert_eq!(g.world().transcriptions, ["ears"]);
    assert_eq!(g.stops(), ["x"], "admitted as the owner's: it evicted");
    assert_eq!(view(&g, "ears").unwrap().owner, Origin::Owner);
}

/// `HEAD /v1/audio/voices?probe=engine` runs the `GET` handler, which
/// admits the speech model: with the header it is a guest as the `GET` is —
/// on a card the owner's idle model fills it defers, and nothing is evicted
/// or started.
#[tokio::test]
async fn a_head_of_the_voice_probe_with_the_header_is_a_guest() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 20 * GIB).await;
    owner_loads(&g, "x").await;
    g.audio("tts", "tts", 6 * GIB).await;

    let resp = gw
        .client()
        .head(format!("{gw}/v1/audio/voices?model=audio/tts&probe=engine"))
        .header(BACKGROUND, "on")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "{:?}", resp.headers());
    assert!(g.stops().is_empty(), "the owner's model was not evicted");
    assert_eq!(g.runs(), ["x"], "nothing started");
}
