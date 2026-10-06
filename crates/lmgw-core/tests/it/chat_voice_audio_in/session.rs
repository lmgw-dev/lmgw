//! Voice turns the model hears, WP3 (voice-audio-input design §3.1–§3.3,
//! §5, §7): a session bound to a thread on `gpu_world`'s local row `gemma`,
//! which takes audio, with the realtime suites' ASR and TTS fakes, over a
//! real socket.
//!
//! - **The hold:** the chat model hears the turn at the commit, while its
//!   transcript is still being made, and nothing of its answer reaches the
//!   client before the transcript; then the user row (named for the
//!   response, before its reply) and the reply, with the timing's fields
//!   and the thread's new title.
//! - **The veto:** a turn that came back without words ends quietly — no
//!   error, no row, no reply.
//! - **A failed transcription**, and the session closing mid-hold, are
//!   `transcripts`.
//! - **A refusal:** the server's refusal of the audio goes again as the
//!   transcript with its note, and the session's later turns go as text.
//! - **A pause in mid-sentence:** the response the first part started is
//!   cut unheard, and one reply answers both parts.
//! - **A fallback:** a cloud fallback that takes audio, which the gate
//!   swaps to after the verdict said "hears you", hears the turn as the
//!   model would have (changed 2026-10-06: capability, not locality; the
//!   fallbacks that cannot are `fallbacks`).
//! - **The GPU claim:** a reply waiting for its user row pins no VRAM.
//! - **`off`:** nothing new is sent or stored.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Notify;

use lmgw_core::config::HoldFallbackMode;
use lmgw_core::store::NewCandidateAlias;

use super::fallbacks::{Cloud, CLOUD_SAYS};
use super::turns::{hears, row};
use crate::realtime_chat_thread::{
    eventually, of_type, say, try_next, until, until_type, world_on, World,
};
use crate::support::gpu_world::{Gpu, ANSWER, GIB};
use crate::support::realtime_audio::{fixture, silence, stream, Asr};
use crate::support::realtime_fakes::{send, Ws};

/// The placeholder a spoken row whose transcription failed goes as.
pub(super) const NOT_TRANSCRIBED: &str = "[spoken turn, not transcribed]";

/// A card with the row `gemma` (it hears, and its template reasons), the
/// realtime fakes beside it, and audio input at `setting`.
pub(super) async fn hearing(setting: &str, total: u64, queue_s: u64) -> (Gpu, World) {
    let g = Gpu::new(total, 3, queue_s).await;
    g.row(row("gemma"), 8 * GIB).await;
    g.world().thinking.insert("gemma".into());
    let setting = setting.to_string();
    let w = world_on(g.state.clone(), move |s| s.chat_voice_audio_input = setting).await;
    (g, w)
}

/// A voice session on a new thread on `gemma`.
pub(super) async fn session(w: &World) -> (i64, Ws) {
    let tid = w.thread("gemma", json!({})).await;
    let ws = w.voice(tid).await;
    (tid, ws)
}

/// Every event that comes within a second of each other.
pub(super) async fn quiet(ws: &mut Ws) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(ev) = try_next(ws, 1).await {
        out.push(ev);
    }
    out
}

/// Every `input_audio` part of `body`, by the index of its message.
fn audio_parts(body: &Value) -> usize {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|p| p["type"] == "input_audio")
        .count()
}

/// The chat frames of `events` named `event`.
fn chat_frames<'a>(events: &'a [Value], event: &str) -> Vec<&'a Value> {
    of_type(events, "lmgw.chat.frame")
        .into_iter()
        .filter(|e| e["event"] == event)
        .collect()
}

/// Nothing of an answer in `events`: no chat output frame, no audio, no
/// user row.
fn no_answer(events: &[Value]) {
    for f in ["delta", "reasoning", "done", "error"] {
        assert!(chat_frames(events, f).is_empty(), "{f} in {events:?}");
    }
    for t in [
        "response.output_audio.delta",
        "lmgw.chat.user",
        "lmgw.chat.reply",
        "error",
    ] {
        assert!(of_type(events, t).is_empty(), "{t} in {events:?}");
    }
}

/// The thread's rows as stored: `(id, role, content, voice)`.
pub(super) async fn rows(w: &World, tid: i64) -> Vec<(i64, String, String, Value)> {
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["id"].as_i64().unwrap(),
                m["role"].as_str().unwrap().to_string(),
                m["content"].as_str().unwrap().to_string(),
                m["voice"].clone(),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_model_hears_the_turn_at_once_and_its_answer_waits_for_the_transcript() {
    let (g, w) = hearing("on", 24 * GIB, 30).await;
    let (tid, mut ws) = session(&w).await;
    let release = Arc::new(Notify::new());
    w.asr
        .push(Asr::HeldText(release.clone(), "Wie spät ist es?"));
    say(&mut ws).await;
    eventually("the model to hear the turn", || async {
        !g.world().streamed_bodies.is_empty()
    })
    .await;
    let held = quiet(&mut ws).await;
    let input = of_type(&held, "lmgw.chat.input");
    assert_eq!(input.len(), 1, "{held:?}");
    assert_eq!(input[0]["input"], "audio");
    assert!(input[0].get("why").is_none(), "{input:?}");
    no_answer(&held);
    let body = g.world().streamed_bodies[0].clone();
    assert_eq!(audio_parts(&body), 1, "the turn went as audio: {body}");

    release.notify_one();
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let rid = of_type(&held, "response.created")[0]["response"]["id"].clone();
    assert_eq!(input[0]["response_id"], rid);
    let done = of_type(&events, "response.done");
    assert_eq!(done[0]["response"]["status"], "completed", "{events:?}");
    // The user row, named for its response.
    let user = of_type(&events, "lmgw.chat.user");
    assert_eq!(user.len(), 1, "{events:?}");
    assert_eq!(user[0]["content"], "Wie spät ist es?");
    assert_eq!(user[0]["response_id"], rid);
    assert_eq!(user[0]["voice"]["input"], "audio");
    // The untitled thread is named from it, and the page told.
    let named = of_type(&events, "lmgw.chat.thread");
    assert_eq!(named.len(), 1, "{events:?}");
    assert_eq!(named[0]["chat_thread"]["title"], "Wie spät ist es?");
    // The timing: heard as audio, and how long its first output waited.
    let timing = events.last().unwrap();
    assert_eq!(timing["input"], "audio", "{timing}");
    assert!(timing.get("input_why").is_none(), "{timing}");
    assert!(
        timing["transcript_wait_ms"].as_u64().unwrap() > 0,
        "{timing}"
    );
    // The row comes before the reply, and holds the transcript.
    let r = rows(&w, tid).await;
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(
        (r[0].1.as_str(), r[0].2.as_str()),
        ("user", "Wie spät ist es?")
    );
    assert_eq!((r[1].1.as_str(), r[1].2.as_str()), ("assistant", ANSWER));
    assert!(r[0].0 < r[1].0, "{r:?}");
    assert_eq!(r[0].3["input"], "audio");
    assert!(r[0].3.get("transcript_error").is_none());
    assert_eq!(r[1].3["timing"]["input"], "audio");

    // The next turn hears its own audio; the first replays as its text.
    w.asr.push(Asr::Text("Und morgen?"));
    say(&mut ws).await;
    until_type(&mut ws, "lmgw.response.timing").await;
    let body = g.world().streamed_bodies[1].clone();
    assert_eq!(audio_parts(&body), 1, "{body}");
    let texts: Vec<&Value> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| &m["content"])
        .collect();
    assert!(texts.contains(&&json!("Wie spät ist es?")), "{body}");
}

#[tokio::test]
async fn noise_ends_quietly_with_no_row_and_no_reply() {
    let (g, w) = hearing("on", 24 * GIB, 30).await;
    let (tid, mut ws) = session(&w).await;
    // The transcript comes back empty after the model streamed its answer.
    let release = Arc::new(Notify::new());
    w.asr.push(Asr::HeldText(release.clone(), ""));
    say(&mut ws).await;
    eventually("the model to hear the turn", || async {
        !g.world().streamed_bodies.is_empty()
    })
    .await;
    release.notify_one();
    let events = until_type(&mut ws, "response.done").await;
    let done = &of_type(&events, "response.done")[0]["response"];
    assert_eq!(done["status"], "cancelled", "{events:?}");
    assert_eq!(done["status_details"]["reason"], "no_words");
    let mut all = events.clone();
    all.extend(quiet(&mut ws).await);
    no_answer(&all);
    assert!(of_type(&all, "lmgw.response.timing").is_empty(), "{all:?}");
    assert_eq!(g.world().streamed_bodies.len(), 1, "the model did hear it");
    assert!(rows(&w, tid).await.is_empty(), "nothing is written");
}

#[tokio::test]
async fn a_refused_audio_goes_again_as_its_transcript_and_the_session_remembers() {
    let (g, w) = hearing("on", 24 * GIB, 30).await;
    g.world().refuse_audio.insert("gemma".into(), 500);
    let (tid, mut ws) = session(&w).await;
    w.asr.push(Asr::Text("Wie spät ist es?"));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(
        of_type(&events, "response.done")[0]["response"]["status"],
        "completed",
        "{events:?}"
    );
    let input = of_type(&events, "lmgw.chat.input");
    assert_eq!(input.len(), 2, "{events:?}");
    assert_eq!(input[0]["input"], "audio");
    assert_eq!(input[1]["input"], "transcript");
    let why = input[1]["why"].as_str().unwrap();
    assert!(why.contains("audio input is not supported"), "{why}");
    assert!(chat_frames(&events, "turn").is_empty(), "{events:?}");
    assert!(chat_frames(&events, "error").is_empty(), "{events:?}");
    assert!(of_type(&events, "error").is_empty(), "{events:?}");
    let timing = events.last().unwrap();
    assert_eq!(timing["input"], "transcript", "{timing}");
    assert!(timing["input_why"].as_str().unwrap().contains("refused"));
    let sent = g.world().streamed_bodies.clone();
    assert_eq!(sent.len(), 2, "the audio, then the transcript");
    assert_eq!((audio_parts(&sent[0]), audio_parts(&sent[1])), (1, 0));
    let r = rows(&w, tid).await;
    assert_eq!(
        r.iter().map(|x| x.2.as_str()).collect::<Vec<_>>(),
        ["Wie spät ist es?", ANSWER]
    );

    // The session's next turn goes as its transcript at once.
    w.asr.push(Asr::Text("Und morgen?"));
    say(&mut ws).await;
    let events = until_type(&mut ws, "lmgw.response.timing").await;
    let input = of_type(&events, "lmgw.chat.input");
    assert_eq!(input.len(), 1, "{events:?}");
    assert_eq!(input[0]["input"], "transcript");
    assert!(input[0]["why"]
        .as_str()
        .unwrap()
        .starts_with("it refused the audio this session"));
    let sent = g.world().streamed_bodies.clone();
    assert_eq!(sent.len(), 3);
    assert_eq!(audio_parts(&sent[2]), 0);
}

/// Server VAD at a short silence: the TTS-generated fixture's two
/// sentences commit as two turns.
async fn vad(ws: &mut Ws) {
    send(
        ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "audio": {"input": {"turn_detection": {"type": "server_vad",
                "silence_duration_ms": 300}}}}}),
    )
    .await;
    until_type(ws, "session.updated").await;
}

#[tokio::test]
async fn a_pause_in_mid_sentence_gets_one_reply_to_both_parts() {
    let (g, w) = hearing("on", 24 * GIB, 30).await;
    let (tid, mut ws) = session(&w).await;
    vad(&mut ws).await;
    let release = Arc::new(Notify::new());
    w.asr.push(Asr::HeldText(release.clone(), "Ich bin müde."));
    w.asr.push(Asr::Text("Was soll ich tun?"));
    stream(&mut ws, &fixture("en_two_sentences_pause.wav")).await;
    stream(&mut ws, &silence(800)).await;
    let mut events = until_type(&mut ws, "input_audio_buffer.committed").await;
    events.extend(until_type(&mut ws, "input_audio_buffer.committed").await);
    release.notify_one();
    events.extend(until_type(&mut ws, "lmgw.response.timing").await);
    events.extend(quiet(&mut ws).await);
    let done = of_type(&events, "response.done");
    assert_eq!(done.len(), 2, "{events:?}");
    assert_eq!(done[0]["response"]["status"], "cancelled");
    assert_eq!(
        done[0]["response"]["status_details"]["reason"],
        "turn_detected"
    );
    assert_eq!(done[1]["response"]["status"], "completed");
    // Nothing of the first response reached the client.
    let first = done[0]["response"]["id"].clone();
    assert!(
        of_type(&events, "lmgw.chat.frame")
            .iter()
            .all(|f| f["response_id"] != first || f["event"] == "state"),
        "{events:?}"
    );
    // The last request answers both: the first part as its row, the second
    // as audio, in one user message. (The cut one may have been sent or
    // not: it was cut as soon as the user went on.)
    let sent = g.world().streamed_bodies.clone();
    assert!(!sent.is_empty() && sent.len() <= 2, "{sent:?}");
    let last = sent.last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    let parts = last["content"].as_array().expect("merged parts");
    assert_eq!(parts[0]["text"], "Ich bin müde.", "{last}");
    assert_eq!(parts[1]["type"], "input_audio", "{last}");
    // One reply, after both rows.
    let r = rows(&w, tid).await;
    let shape: Vec<(&str, &str)> = r.iter().map(|x| (x.1.as_str(), x.2.as_str())).collect();
    assert_eq!(
        shape,
        [
            ("user", "Ich bin müde."),
            ("user", "Was soll ich tun?"),
            ("assistant", ANSWER)
        ]
    );
}

#[tokio::test]
async fn a_reply_waiting_for_its_user_row_pins_no_vram() {
    // Room for one of the two models, and a short queue: a claim still held
    // would time the second request out.
    let (g, w) = hearing("on", 10 * GIB, 3).await;
    // (Not `other`: the realtime fakes name a chat alias so.)
    g.model("second", 8 * GIB).await;
    let (tid, mut ws) = session(&w).await;
    let release = Arc::new(Notify::new());
    w.asr
        .push(Asr::HeldText(release.clone(), "Wie spät ist es?"));
    say(&mut ws).await;
    eventually("the model to hear the turn", || async {
        !g.world().streamed_bodies.is_empty()
    })
    .await;
    quiet(&mut ws).await;
    // The answer is in, and its save waits for the row: `gemma` is idle.
    let r =
        w.gw.client()
            .post(format!("{}/v1/chat/completions", w.gw))
            .json(&json!({"model": "second", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    release.notify_one();
    until_type(&mut ws, "lmgw.response.timing").await;
    let r = rows(&w, tid).await;
    assert_eq!(
        r.iter().map(|x| x.2.as_str()).collect::<Vec<_>>(),
        ["Wie spät ist es?", ANSWER]
    );
}

/// The whole event stream with the setting off (WP3 review #10): no event
/// WP3 added, and no field of one — the `off` golden (`managed_off`)
/// guards the shapes of the chat events, this every event.
#[tokio::test]
async fn with_audio_input_off_nothing_new_is_sent_or_stored() {
    let (g, w) = hearing("off", 24 * GIB, 30).await;
    let (tid, mut ws) = session(&w).await;
    w.asr.push(Asr::Text("Wie spät ist es?"));
    say(&mut ws).await;
    let mut events = until(&mut ws, |e| e["type"] == "lmgw.response.timing").await;
    events.extend(quiet(&mut ws).await);
    assert!(of_type(&events, "lmgw.chat.input").is_empty(), "{events:?}");
    for ev in &events {
        let text = ev.to_string();
        for k in [
            "\"input_why\"",
            "\"transcript_wait_ms\"",
            "\"transcript_error\"",
            "\"input\":\"audio\"",
            "\"input\":\"transcript\"",
        ] {
            assert!(!text.contains(k), "{k} in {ev}");
        }
        if ev["type"] == "lmgw.chat.user" {
            assert!(ev.get("response_id").is_none(), "{ev}");
        }
        if ev["type"] == "response.done" {
            assert_ne!(ev["response"]["status_details"]["reason"], "no_words");
        }
    }
    // The turn was transcribed before the response launched, as today.
    let committed = events
        .iter()
        .position(|e| e["type"] == "conversation.item.input_audio_transcription.completed")
        .expect("transcribed");
    let created = events
        .iter()
        .position(|e| e["type"] == "response.created")
        .expect("a response");
    let first_frame = events
        .iter()
        .position(|e| e["type"] == "lmgw.chat.frame" && e["event"] != "state")
        .expect("its turn");
    assert!(
        committed < first_frame && created < first_frame,
        "{events:?}"
    );
    let user = of_type(&events, "lmgw.chat.user");
    let mut keys: Vec<&str> = user[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["content", "event_id", "message_id", "type", "voice"]);
    assert!(user[0]["voice"].get("input").is_none(), "{user:?}");
    let timing = events.last().unwrap();
    for k in ["input", "input_why", "transcript_wait_ms"] {
        assert!(timing.get(k).is_none(), "{k} in {timing}");
    }
    assert_eq!(audio_parts(&g.world().streamed_bodies[0]), 0);
    let r = rows(&w, tid).await;
    assert!(r[0].3.get("input").is_none(), "{r:?}");
    assert!(r[1].3["timing"].get("input").is_none(), "{r:?}");
}

#[tokio::test]
async fn a_cloud_fallback_the_gate_swaps_to_hears_the_turn() {
    let (g, w) = hearing("on", 24 * GIB, 30).await;
    let cloud = Cloud::new(&g, hears(), false).await;
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
    // A game holds the card: admission's outside-VRAM verdict hands the turn
    // to the cloud fallback, which the verdict (`gate::resolve`) does not
    // foresee. The fallback takes audio: it hears the turn.
    {
        let mut gw = g.world();
        gw.attribution = true;
        gw.outside = 20 * GIB;
    }
    let tid = w.thread("writer", json!({})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Wie spät ist es?"));
    say(&mut ws).await;
    let mut events = until_type(&mut ws, "lmgw.response.timing").await;
    events.extend(quiet(&mut ws).await);
    let input = of_type(&events, "lmgw.chat.input");
    assert_eq!(input.len(), 1, "{events:?}");
    assert_eq!(input[0]["input"], "audio");
    assert!(chat_frames(&events, "error").is_empty(), "{events:?}");
    let sent = cloud.bodies().await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(audio_parts(&sent[0]), 1, "the fallback heard it");
    assert!(g.world().streamed_bodies.is_empty(), "nothing ran locally");
    let timing = events.last().unwrap();
    assert_eq!(timing["input"], "audio", "{timing}");
    assert!(timing.get("transcript_wait_ms").is_some(), "{timing}");
    // The row holds the transcript and says the model heard it; the reply
    // follows it.
    let r = rows(&w, tid).await;
    let shape: Vec<(&str, &str)> = r.iter().map(|x| (x.1.as_str(), x.2.as_str())).collect();
    assert_eq!(
        shape,
        [("user", "Wie spät ist es?"), ("assistant", CLOUD_SAYS)]
    );
    assert_eq!(r[0].3["input"], "audio", "{r:?}");
}

/// The skipped attempt (WP2 review): the thread's setting went off after
/// the verdict was judged, so the responder skips the audio and waits for
/// the row — which the journal writes because the attempt says no turn
/// began. Before, the two waited on each other.
#[tokio::test]
async fn a_turn_whose_setting_went_off_since_the_verdict_goes_as_its_transcript() {
    let (g, w) = hearing("on", 24 * GIB, 30).await;
    let (tid, mut ws) = session(&w).await;
    // Push-to-talk judges no verdict before the first turn: the bind's
    // ("hears you") stands.
    w.set(tid, json!({"voice": {"audio_input": "off"}})).await;
    w.asr.push(Asr::Text("Wie spät ist es?"));
    say(&mut ws).await;
    let events = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        until_type(&mut ws, "lmgw.response.timing"),
    )
    .await
    .expect("no hang");
    assert_eq!(
        of_type(&events, "response.done")[0]["response"]["status"],
        "completed",
        "{events:?}"
    );
    let input = of_type(&events, "lmgw.chat.input");
    assert_eq!(input.len(), 2, "{events:?}");
    assert_eq!(input[1]["why"], "audio input is off (this thread)");
    let sent = g.world().streamed_bodies.clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(audio_parts(&sent[0]), 0, "the transcript only");
    let r = rows(&w, tid).await;
    assert_eq!(
        r.iter().map(|x| x.2.as_str()).collect::<Vec<_>>(),
        ["Wie spät ist es?", ANSWER]
    );
}
