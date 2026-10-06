//! Voice turns the model hears, WP2 (voice-audio-input design §3.4, §3.5,
//! §7): the request a heard turn makes, on `support/gpu_world.rs`'s local
//! rows — a model lmgw runs, behind the real gate. The new turn alone goes
//! as audio and earlier spoken turns as text; a tool thread's every model
//! call carries it and its record never does; a cloud fallback that takes
//! audio, which the gate swaps to at admission or a guest's re-pick hands
//! it to, gets it (changed 2026-10-06: capability, not locality; the
//! fallbacks that cannot are `fallbacks`); and a managed server's refusal
//! of the audio goes once more as the
//! transcript, with the note — kept by the session when the refusal names
//! the audio or the transcript retry answered, never after a failed retry.
//! Driven through the turn seam (`web::spoken_turn_for_tests`) and the
//! bound responder (`realtime::heard_response_for_tests`); the core and the
//! journal that feed them are WP3's.

use lmgw_core::config::{HoldFallbackMode, SelfAdmin};
use lmgw_core::ir::ContentPart;
use lmgw_core::realtime::heard_response_for_tests;
use lmgw_core::store::{self, NewCandidateAlias, NewLocalModel};
use lmgw_core::web::spoken_turn_for_tests;
use serde_json::{json, Value};

use crate::support::gpu_world::{Gpu, ANSWER, GIB};

/// A synthetic WAV's base64 — a header, no speech: nothing here is a
/// voice.
const WAV: &str = "UklGRiQAAABXQVZFZm10IBAAAAABAAEAgD4AAAB9AAACABAAZGF0YQAAAAA=";

pub(super) fn audio() -> ContentPart {
    ContentPart::Audio {
        mime: "audio/wav".into(),
        data: WAV.into(),
    }
}

/// Capabilities that take audio, stated whole (the sparse weights carry no
/// header to read them from).
pub(super) fn hears() -> Value {
    json!({"capabilities": {
        "task": "chat",
        "endpoints": ["/v1/chat/completions"],
        "input_modalities": ["text", "audio"],
        "source": "owner",
    }})
}

/// A public local chat row `id` that takes audio.
pub(super) fn row(id: &str) -> NewLocalModel {
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
        capabilities_override: Some(hears()),
        ladder: vec![],
    }
}

/// A card with the row `gemma`, which hears and streams, and the setting at
/// `on`.
pub(super) async fn world() -> Gpu {
    let g = Gpu::new(24 * GIB, 2, 2).await;
    g.row(row("gemma"), 8 * GIB).await;
    g.world().thinking.insert("gemma".into());
    let mut s = g.state.snapshot().settings.clone();
    s.chat_voice_audio_input = "on".into();
    s.self_admin = SelfAdmin::ReadOnly;
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
    g
}

/// A thread of `kind` on `model` whose history is an earlier spoken turn
/// and its reply.
pub(super) async fn thread(g: &Gpu, model: &str, kind: &str) -> i64 {
    let tid = store::create_chat_thread(&g.state.db, model, kind)
        .await
        .unwrap();
    for (role, text) in [("user", "Wie spät ist es?"), ("assistant", "Keine Ahnung.")] {
        store::append_chat_message(&g.state.db, tid, role, text, "", None, None, None)
            .await
            .unwrap();
    }
    tid
}

/// The journal's row for the turn: an insert that moves no generation.
pub(super) async fn row_written(g: &Gpu, tid: i64, text: &str) -> i64 {
    store::append_chat_message(&g.state.db, tid, "user", text, "", None, None, None)
        .await
        .unwrap()
}

/// Every `input_audio` part of `body`, by the index of its message.
fn audio_parts(body: &Value) -> Vec<usize> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .flat_map(|(i, m)| {
            m["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["type"] == "input_audio")
                .map(move |_| i)
        })
        .collect()
}

fn streamed(g: &Gpu) -> Vec<Value> {
    g.world().streamed_bodies.clone()
}

fn last_frame(frames: &[(String, Value)]) -> &Value {
    let (event, data) = frames.last().expect("frames");
    assert_eq!(event, "done", "{frames:?}");
    data
}

#[tokio::test]
async fn the_new_turn_alone_goes_as_audio_to_the_model_lmgw_runs() {
    let g = world().await;
    let tid = thread(&g, "gemma", "chat").await;
    let frames = spoken_turn_for_tests(&g.state, tid, vec![audio()]).await;
    assert_eq!(last_frame(&frames)["saved"], true, "{frames:?}");
    assert!(!frames.iter().any(|(e, _)| e == "turn"), "{frames:?}");
    let sent = streamed(&g);
    assert_eq!(sent.len(), 1, "{sent:?}");
    let msgs = sent[0]["messages"].as_array().unwrap();
    assert_eq!(
        audio_parts(&sent[0]),
        [msgs.len() - 1],
        "the last message only"
    );
    assert_eq!(
        msgs.last().unwrap()["content"],
        json!([{"type": "input_audio", "input_audio": {"data": WAV, "format": "wav"}}])
    );
    // The earlier spoken turn replays as its transcript.
    assert_eq!(msgs[msgs.len() - 3]["content"], "Wie spät ist es?");
    // Nothing of the audio is kept.
    let rows = store::list_chat_messages(&g.state.db, tid).await.unwrap();
    let reply = rows.last().unwrap();
    assert_eq!(reply.content, ANSWER);
    assert!(reply.ir_messages.is_none());
    assert!(rows.iter().all(|m| !m.content.contains(WAV)));
}

#[tokio::test]
async fn a_tool_threads_every_model_call_hears_the_turn_and_its_record_never_does() {
    let g = world().await;
    g.world()
        .calls_tool
        .insert("gemma".into(), "lmgw__mcp_servers".into());
    let tid = thread(&g, "gemma", "admin").await;
    let frames = spoken_turn_for_tests(&g.state, tid, vec![audio()]).await;
    assert_eq!(last_frame(&frames)["saved"], true, "{frames:?}");
    assert!(frames
        .iter()
        .any(|(e, d)| e == "tool" && d["event"] == "result"));
    let sent = streamed(&g);
    assert_eq!(sent.len(), 2, "the call, then the answer: {sent:?}");
    for body in &sent {
        assert_eq!(audio_parts(body).len(), 1, "{body}");
    }
    let rows = store::list_chat_messages(&g.state.db, tid).await.unwrap();
    let reply = rows.last().unwrap();
    let record = reply
        .ir_messages
        .as_deref()
        .expect("the tool call's record");
    assert!(record.contains("lmgw__mcp_servers"), "{record}");
    for leak in ["input_audio", "\"audio\"", WAV] {
        assert!(!record.contains(leak), "{leak} in {record}");
    }
}

/// The gate's swap at admission (§4.7's outside-VRAM verdict) hands the
/// turn to a cloud fallback after the verdict said "hears you": the
/// fallback takes audio, so it hears the turn (changed 2026-10-06), and
/// nothing is started.
#[tokio::test]
async fn a_cloud_fallback_the_gate_swaps_to_at_admission_gets_the_audio() {
    let g = world().await;
    g.cloud("cloud", Some(hears())).await;
    g.candidate(NewCandidateAlias {
        alias: "writer".into(),
        candidates: vec!["gemma".into()],
        background: false,
        fallback_mode: HoldFallbackMode::Alias,
        fallback: Some("cloud".into()),
        capabilities_disabled: vec![],
        capabilities_enabled: vec!["audio".into()],
        enabled: true,
        notes: String::new(),
    })
    .await;
    {
        let mut w = g.world();
        w.attribution = true;
        w.outside = 20 * GIB;
    }
    let tid = thread(&g, "writer", "chat").await;
    let frames = spoken_turn_for_tests(&g.state, tid, vec![audio()]).await;
    assert!(
        !frames.iter().any(|(e, _)| e == "error"),
        "not refused: {frames:?}"
    );
    let cloud = g.cloud_bodies().await;
    assert_eq!(cloud.len(), 1, "{cloud:?}");
    let msgs = cloud[0]["messages"].as_array().unwrap();
    assert_eq!(audio_parts(&cloud[0]), [msgs.len() - 1], "the new turn");
    assert!(streamed(&g).is_empty() && g.runs().is_empty());
}

/// A guest's candidate that cannot hold the request (llama-server's context
/// refusal, held back for a re-pick) hands it to the alias's cloud fallback
/// before anything is answered (WP2 review #8): that fallback takes audio,
/// so it hears the turn, on the plain path and in the tool loop, whose
/// re-route is judged inside the loop (`PerRoute::request`).
#[tokio::test]
async fn a_guests_repick_to_its_cloud_fallback_gets_the_audio() {
    // Hears and calls tools, as a tool thread's alias must promise.
    let tools = || {
        let mut c = hears();
        c["capabilities"]["tool_calls"] = json!({"kind": "native"});
        c
    };
    for kind in ["chat", "admin"] {
        let g = world().await;
        g.cloud("cloud", Some(tools())).await;
        g.row(
            NewLocalModel {
                capabilities_override: Some(tools()),
                ..row("tooly")
            },
            8 * GIB,
        )
        .await;
        g.candidate(NewCandidateAlias {
            alias: "helper".into(),
            candidates: vec!["tooly".into()],
            background: true,
            fallback_mode: HoldFallbackMode::Alias,
            fallback: Some("cloud".into()),
            capabilities_disabled: vec![],
            capabilities_enabled: vec!["audio".into(), "tool_calls".into()],
            enabled: true,
            notes: String::new(),
        })
        .await;
        g.world().refuse_context.insert("tooly".into());
        let tid = thread(&g, "helper", kind).await;
        let frames = spoken_turn_for_tests(&g.state, tid, vec![audio()]).await;
        assert!(
            !frames.iter().any(|(e, _)| e == "error"),
            "{kind}: not refused: {frames:?}"
        );
        let sent = streamed(&g);
        assert_eq!(
            sent.len(),
            1,
            "{kind}: the candidate heard it once: {sent:?}"
        );
        assert_eq!(audio_parts(&sent[0]).len(), 1);
        let cloud = g.cloud_bodies().await;
        assert_eq!(cloud.len(), 1, "{kind}: {cloud:?}");
        assert_eq!(
            audio_parts(&cloud[0]).len(),
            1,
            "{kind}: the fallback heard it"
        );
    }
}

/// The bound responder's retry (§3.5): a heard response of thread `tid`
/// whose row the journal wrote as `text`.
async fn heard(g: &Gpu, tid: i64, text: &str) -> lmgw_core::realtime::HeardForTests {
    let id = row_written(g, tid, text).await;
    heard_response_for_tests(&g.state, tid, vec![audio()], id).await
}

#[tokio::test]
async fn a_server_that_refuses_the_audio_hears_the_transcript_once_with_a_note() {
    let g = world().await;
    g.world().refuse_audio.insert("gemma".into(), 500);
    let tid = thread(&g, "gemma", "chat").await;
    let out = heard(&g, tid, "Und morgen?").await;
    assert_eq!(out.result, Ok(ANSWER.to_string()), "{out:?}");
    let events: Vec<&str> = out.frames.iter().map(|(e, _)| e.as_str()).collect();
    assert!(
        !events.contains(&"turn") && !events.contains(&"error"),
        "{events:?}"
    );
    assert_eq!(out.notes.len(), 1, "{out:?}");
    let why = &out.notes[0];
    assert!(why.contains("audio input is not supported"), "{why}");
    // It names the audio: the session keeps it for the model, at once.
    assert_eq!(out.remembered.len(), 1, "{out:?}");
    let (model, kept, note) = &out.remembered[0];
    assert_eq!(model, "gemma");
    assert!(
        kept.starts_with("it refused the audio this session:")
            && kept.contains("audio input is not supported"),
        "{kept}"
    );
    assert!(note.is_none(), "the retry's note said it");
    let sent = streamed(&g);
    assert_eq!(sent.len(), 2, "the audio, then the transcript: {sent:?}");
    assert_eq!(audio_parts(&sent[0]).len(), 1);
    assert!(audio_parts(&sent[1]).is_empty());
    let last = sent[1]["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["content"], "Und morgen?", "{last}");
    let rows = store::list_chat_messages(&g.state.db, tid).await.unwrap();
    let tail: Vec<_> = rows
        .iter()
        .rev()
        .take(2)
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(tail, [ANSWER, "Und morgen?"]);
}

#[tokio::test]
async fn a_second_refusal_is_the_error_and_an_unavailable_server_is_not_retried() {
    // Refused for its length, and the transcript too: one retry, its error.
    let g = world().await;
    g.world().refuse_context.insert("gemma".into());
    let tid = thread(&g, "gemma", "chat").await;
    let out = heard(&g, tid, "Und morgen?").await;
    assert_eq!(out.result, Err("context_length_exceeded".into()), "{out:?}");
    assert_eq!(out.notes.len(), 1);
    assert_eq!(streamed(&g).len(), 2, "no third attempt");
    // The transcript met it as well: nothing about the audio, nothing kept
    // (WP2 review #3).
    assert!(out.remembered.is_empty(), "{out:?}");

    // A 503 is the server's state, not the audio's: the error, at once.
    let g = world().await;
    g.world().refuse_audio.insert("gemma".into(), 503);
    let tid = thread(&g, "gemma", "chat").await;
    let out = heard(&g, tid, "Und morgen?").await;
    assert_eq!(out.result, Err("upstream".into()), "{out:?}");
    assert!(out.notes.is_empty());
    assert!(out.remembered.is_empty());
    assert!(out.frames.iter().any(|(e, _)| e == "error"), "{out:?}");
    assert_eq!(streamed(&g).len(), 1);
}

/// A refusal that does not name the audio — a 400 "Loading model" — is
/// retried, and kept only once the transcript retry answered: then it was
/// about the audio (WP2 review #3).
#[tokio::test]
async fn a_refusal_the_transcript_retry_answered_is_kept_for_the_model() {
    let g = world().await;
    g.world().refuse_audio.insert("gemma".into(), 400);
    let tid = thread(&g, "gemma", "chat").await;
    let out = heard(&g, tid, "Und morgen?").await;
    assert_eq!(out.result, Ok(ANSWER.to_string()), "{out:?}");
    assert_eq!(out.notes.len(), 1, "{out:?}");
    assert!(
        out.notes[0].contains("refused the audio"),
        "{:?}",
        out.notes
    );
    assert_eq!(out.remembered.len(), 1, "{out:?}");
    assert_eq!(out.remembered[0].0, "gemma");
    let sent = streamed(&g);
    assert_eq!(sent.len(), 2, "the audio, then the transcript: {sent:?}");
}
