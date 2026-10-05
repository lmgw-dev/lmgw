//! The `off` golden on a managed server (voice-audio-input design §7, WP2
//! review #8): with audio input off, a voice turn of a session bound to a
//! thread on a model lmgw runs (`gpu_world`'s `gemma`, a llama-server body
//! with its own fields) sends the body it sent before the feature, and the
//! session's chat events carry the fields they carried: compared with
//! `tests/fixtures/chat_requests/managed_off.json`. The events are kept as
//! their shape — type, the chat frame's event, the keys — since ids and
//! times change from run to run, and the system prompt's date is masked. `LMGW_BLESS=1` rewrites the file (the diff
//! then shows what changed); without it, a missing file fails.
//!
//! Captured when the review fixes landed, not before WP2: the turn seam's
//! goldens (`chat_turn/seam_tests/spoken.rs`, captured at 1e1475f) prove the
//! request with no spoken parts unchanged through WP2, and a body with no
//! spoken parts goes through the same `build_messages`; this one guards a
//! managed server's body and the session's events from here on.

use serde_json::{json, Value};

use super::session::{hearing, session};
use crate::realtime_chat_thread::{say, until_type};
use crate::support::gpu_world::GIB;
use crate::support::realtime_audio::Asr;

/// An event as its shape: its type, a chat frame's event, and the sorted
/// keys of the event, its `data` and its `voice`.
fn shape(ev: &Value) -> Option<Value> {
    let keys = |v: &Value| {
        let mut k: Vec<String> = v
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        k.sort_unstable();
        k
    };
    let t = ev["type"].as_str()?;
    if !t.starts_with("lmgw.chat.") && t != "lmgw.response.timing" {
        return None;
    }
    Some(json!({
        "type": t,
        "event": ev.get("event"),
        "keys": keys(ev),
        "data": keys(&ev["data"]),
        "voice": keys(&ev["voice"]),
    }))
}

/// `body` with its system prompt's date masked: it changes every day.
fn undated(mut body: Value) -> Value {
    for m in body["messages"].as_array_mut().into_iter().flatten() {
        let Some(c) = m["content"].as_str() else {
            continue;
        };
        let Some(at) = c.find("Today is ") else {
            continue;
        };
        let end = c[at..].find(". ").map_or(c.len(), |e| at + e + 1);
        m["content"] = Value::String(format!("{}Today is <date>.{}", &c[..at], &c[end..]));
    }
    body
}

/// Compare `got` with the golden `name`.
fn golden(name: &str, got: &Value) {
    let got = serde_json::to_string_pretty(got).unwrap() + "\n";
    let file = format!(
        "{}/tests/fixtures/chat_requests/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    if std::env::var_os("LMGW_BLESS").is_some() {
        std::fs::write(&file, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&file)
        .unwrap_or_else(|e| panic!("{file}: {e} — run the test with LMGW_BLESS=1 to capture it"));
    assert!(
        got == want,
        "'{name}' changed\n--- want ({file})\n{want}\n--- got\n{got}"
    );
}

#[tokio::test]
async fn with_audio_input_off_a_managed_server_gets_todays_body_and_events() {
    let (g, w) = hearing("off", 24 * GIB, 30).await;
    let (_tid, mut ws) = session(&w).await;
    w.asr.push(Asr::Text("Wie spät ist es?"));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let bodies: Vec<Value> = g
        .world()
        .streamed_bodies
        .iter()
        .cloned()
        .map(undated)
        .collect();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    let shapes: Vec<Value> = events.iter().filter_map(shape).collect();
    golden(
        "managed_off",
        &json!({ "bodies": bodies, "events": shapes }),
    );
}
