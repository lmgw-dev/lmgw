//! The thread's models in a bound session (§4.1, §4.4, §8.7): the connect
//! warm admits them as the press does — evicting an idle model where a
//! Background warm would skip — and under the GPU hold the mic's audio
//! goes to the ASR's fallback, named in `lmgw.model.state` and the timing.
//! On the GPU-world fake.

use serde_json::{json, Value};

use super::{of_type, say, until, until_type, world_on, World};
use crate::chat_voice_dictation::{asr_row, cloud_asr, resident, tweak};
use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_fakes::Turn;

/// A GPU world of `total` bytes with its audio models dir set, and the
/// harness's fakes on its state.
async fn gpu(total: u64, containers: usize) -> (Gpu, World) {
    let g = Gpu::new(total, containers, 30).await;
    let models = g.models_dir().display().to_string();
    tweak(&g.state, |s| s.audio.models_dir = models).await;
    let w = world_on(g.state.clone(), |_| {}).await;
    (g, w)
}

/// The `lmgw.model.state` events of `stage`, as `(state, ms or
/// answered_by)`.
fn states(events: &[Value], stage: &str) -> Vec<Value> {
    of_type(events, "lmgw.model.state")
        .into_iter()
        .filter(|e| e["stage"] == stage)
        .cloned()
        .collect()
}

#[tokio::test]
async fn the_connect_warm_admits_the_thread_s_models() {
    let (g, w) = gpu(10 * GIB, 3).await;
    g.model("other", 6 * GIB).await;
    g.model("talk", 6 * GIB).await;
    // Idle and resident: a Background warm would find no room and skip.
    resident(&g, "other").await;
    asr_row(&g, "ears", true, None).await;
    let tid = w.thread("talk", json!({})).await;
    w.set(
        tid,
        json!({"voice": {"asr_alias": "audio/ears", "language": "de"}}),
    )
    .await;
    let (mut ws, _) = w.bind(tid).await;
    let events = until(&mut ws, |e| {
        e["type"] == "lmgw.model.state" && e["stage"] == "chat" && e["state"] == "ready"
    })
    .await;
    let chat = states(&events, "chat");
    assert_eq!(chat[0]["state"], "loading", "{chat:?}");
    assert!(chat.last().unwrap()["ms"].is_u64(), "{chat:?}");
    assert!(g.runs().contains(&"talk".to_string()), "{:?}", g.runs());
    assert_eq!(g.stops(), ["other"], "the press evicted the idle model");
}

#[tokio::test]
async fn under_the_hold_the_asr_fallback_hears_and_is_named() {
    let (g, w) = gpu(10 * GIB, 2).await;
    let _cloud = cloud_asr(&g.state, "cloud-asr", "Hallo aus der Wolke.").await;
    asr_row(&g, "gpu-ears", false, Some("cloud-asr")).await;
    tweak(&g.state, |s| s.hold.active = true).await;
    let tid = w.thread("chatty", json!({})).await;
    w.set(
        tid,
        json!({"voice": {"asr_alias": "audio/gpu-ears", "language": "de"}}),
    )
    .await;
    let (mut ws, _) = w.bind(tid).await;
    // The connect warm says where the mic's audio will go.
    let events = until(&mut ws, |e| {
        e["type"] == "lmgw.model.state" && e["stage"] == "asr" && e["state"] != "loading"
    })
    .await;
    super::manual(&mut ws, 60_000).await;
    let asr = states(&events, "asr");
    let last = asr.last().unwrap();
    assert_eq!(
        (&last["state"], &last["answered_by"]),
        (&json!("fallback"), &json!("cloud-asr")),
        "{asr:?}"
    );
    w.chat.push(Turn::text(&["Hallo."]));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let timing = of_type(&events, "lmgw.response.timing")[0];
    assert_eq!(
        timing["models"]["asr"]["alias"], "audio/gpu-ears",
        "{timing}"
    );
    assert_eq!(timing["models"]["asr"]["answered_by"], "cloud-asr");
    let m = w.messages(tid).await;
    assert_eq!(m[0].1, "Hallo aus der Wolke.");
    assert_eq!(m[0].2["asr_answered_by"], "cloud-asr");
    assert!(g.runs().is_empty(), "nothing local started: {:?}", g.runs());
    assert_eq!(w.asr.seen.count(), 0, "the Chat's own ASR heard nothing");
}

/// A CPU ASR row that has answered holds its model: a warm after the
/// ledger's next reading loads nothing again, and the turn's timing does not
/// call it cold. The ledger used to forget a CPU container's residency.
#[tokio::test]
async fn a_cpu_asr_that_answered_stays_loaded_across_the_ledger() {
    let (g, w) = gpu(10 * GIB, 2).await;
    asr_row(&g, "ears", true, None).await;
    let tid = w.thread("chatty", json!({})).await;
    w.set(
        tid,
        json!({"voice": {"asr_alias": "audio/ears", "language": "de"}}),
    )
    .await;
    let first = crate::chat_voice_dictation::warm(&w.gw, tid, &["asr"]).await;
    let asr = crate::chat_voice_dictation::states(&first, "asr");
    assert_eq!(asr[0]["state"], "loading", "{asr:?}");
    // The ledger reads the registry (what any admission or the titlebar
    // does).
    let _ = w.get("/api/vram").await;
    let again = crate::chat_voice_dictation::warm(&w.gw, tid, &["asr"]).await;
    let asr = crate::chat_voice_dictation::states(&again, "asr");
    assert_eq!(asr.len(), 1, "{asr:?}");
    assert_eq!(
        (&asr[0]["state"], &asr[0]["ms"]),
        (&json!("ready"), &Value::Null),
        "nothing had to load: {asr:?}"
    );
    assert_eq!(g.world().transcriptions, ["ears"], "one load, not two");
}

/// A turn's own `state` frames are the session's model state once (WP8
/// review m2): a chat model that has to start says one `loading` and one
/// `ready`, each relayed as `lmgw.chat.frame` too.
#[tokio::test]
async fn a_cold_chat_stage_is_said_once() {
    let (g, w) = gpu(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    let tid = w.thread("talk", json!({})).await;
    let (mut ws, _) = w.bind(tid).await;
    until(&mut ws, |e| {
        e["type"] == "lmgw.model.state" && e["stage"] == "chat" && e["state"] == "ready"
    })
    .await;
    super::manual(&mut ws, 60_000).await;
    // The connect warm loaded it; it goes, so the turn has to start it.
    g.state
        .runtime()
        .stop(lmgw_core::runtime::Class::Chat, "talk", false)
        .await
        .unwrap();
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let chat: Vec<Value> = states(&events, "chat")
        .iter()
        .map(|s| s["state"].clone())
        .collect();
    assert_eq!(chat, [json!("loading"), json!("ready")], "{events:?}");
    let frames: Vec<&Value> = of_type(&events, "lmgw.chat.frame")
        .into_iter()
        .filter(|e| e["event"] == "state")
        .collect();
    assert_eq!(frames.len(), 2, "{frames:?}");
    let timing = of_type(&events, "lmgw.response.timing")[0];
    assert!(
        timing["cold"].as_array().unwrap().contains(&json!("chat")),
        "{timing}"
    );
}

/// WP8 review m5: `speech_started` warms the thread's models as they are
/// now — a chat model changed by its chip since the bind is the one started,
/// while the turn's transcript is still being made.
#[tokio::test]
async fn speech_started_warms_the_thread_s_models_as_they_are_now() {
    use crate::support::realtime_audio::{fixture, silence, stream, Asr};
    let (g, w) = gpu(10 * GIB, 3).await;
    g.model("talk", GIB).await;
    g.model("talk2", GIB).await;
    let tid = w.thread("talk", json!({})).await;
    let (mut ws, _) = w.bind(tid).await;
    until(&mut ws, |e| {
        e["type"] == "lmgw.model.state" && e["stage"] == "chat" && e["state"] == "ready"
    })
    .await;
    w.set(tid, json!({"model_alias": "talk2"})).await;
    crate::support::realtime_fakes::send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["audio"], "lmgw": {"output_lead_ms": 60_000},
            "audio": {"input": {"turn_detection": {"type": "server_vad",
                "silence_duration_ms": 300}}}}}),
    )
    .await;
    until_type(&mut ws, "session.updated").await;
    let asr_hold = std::sync::Arc::new(tokio::sync::Notify::new());
    w.asr.push(Asr::HeldText(asr_hold.clone(), "Hallo."));
    w.chat.push(Turn::text(&["Hallo."]));
    stream(&mut ws, &fixture("en_complete_short.wav")).await;
    stream(&mut ws, &silence(800)).await;
    until_type(&mut ws, "input_audio_buffer.speech_started").await;
    super::eventually("the changed chat model to be warmed", || async {
        g.runs().contains(&"talk2".to_string())
    })
    .await;
    assert_eq!(w.chat.seen.chat_count(), 0, "the warm, not the turn");
    asr_hold.notify_one();
}
