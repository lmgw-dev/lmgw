//! Turns the chat model hears (voice-audio-input design §3.1), on a core
//! bound to a thread whose verdict says `audio`: such a turn has words
//! until it is transcribed, a response does not wait for its transcript,
//! an owed response is decided at the commit, and a due one is dropped
//! when its turns come back empty. A failed transcription stays the
//! session's own only for a turn a model heard (WP3 review #2, #3): the
//! test plays the responder that says whether its attempt carried the
//! audio.

use std::sync::Arc;

use serde_json::Value;

use super::super::super::audio_in::Detected;
use super::super::super::input::{has_words, transcripts_failed, OpenTurn};
use super::super::super::protocol::{ContentPart, Item, ItemStatus, MessageItem, Role};
use super::super::super::responder::Msg;
use super::super::super::session::{Core, SessionInit};
use super::super::super::thread::hearing::Hearing;
use super::super::super::thread::turn::audio::Spoken;
use super::super::super::transcribe::{Done, Wav};
use super::super::super::turn::server_vad::TurnEvent;
use crate::error::GatewayError;
use crate::state::AppState;
use crate::store::InputPath;

/// A core bound to a thread of its own, whose next turn the verdict says
/// goes as audio, its journal running; and its writer's frames.
async fn bound() -> (
    Core,
    futures::channel::mpsc::UnboundedReceiver<axum::extract::ws::Message>,
) {
    use crate::realtime::asr::{AsrResolution, AsrVia};
    use crate::realtime::resolve::{ChatResolution, Via};
    use crate::web::chat_voice::bound;
    let state = AppState::init_for_tests().await.unwrap();
    let thread = crate::store::ChatThread {
        id: 4,
        model_alias: "m".into(),
        ..Default::default()
    };
    let bind = state.chat_live.bind_voice(thread.id, "the dashboard");
    let mut audio_input = bound::audio_input(&state, &thread).await;
    audio_input.value = crate::store::AudioInputMode::On;
    audio_input.verdict.path = InputPath::Audio;
    audio_input.verdict.why = None;
    let binding = super::super::super::thread::Binding {
        thread_id: thread.id,
        title: "t".into(),
        temporary: false,
        admin_tools: false,
        cfg: bound::voice(&state.snapshot(), &thread),
        audio_input,
        warm: vec![],
        guard: Some(bind.guard),
        taken: bind.taken,
        taken_by: bind.taken_by,
        fence: bind.fence,
    };
    let (sink, frames) = futures::channel::mpsc::unbounded();
    let ids = Arc::new(crate::realtime::ids::Ids::new());
    let (out, _drained, _writer) = crate::realtime::writer::spawn(sink, ids.clone());
    let (responder_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (asr_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (score_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let speech = crate::realtime::session::speech::initial(&state)
        .await
        .unwrap();
    let init = SessionInit {
        running: None,
        state,
        ctx: Default::default(),
        requested_model: None,
        chat: ChatResolution {
            alias: Some("m".into()),
            via: Via::Alias,
        },
        asr: AsrResolution {
            alias: None,
            via: AsrVia::Default(None),
            missing: Some("none in this test".into()),
        },
        speech,
        slot: None,
        limits: crate::realtime::Limits::from_settings(&Default::default()),
        bound: Some(binding),
    };
    let mut core = Core::new(init, ids, out, responder_tx, asr_tx, score_tx);
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let (states, _) = tokio::sync::mpsc::unbounded_channel();
    let (verdicts, _) = tokio::sync::mpsc::unbounded_channel();
    core.start_bound(events, states, verdicts);
    core.session.output_modalities = Some(vec![super::super::super::protocol::Modality::Text]);
    (core, frames)
}

/// A turn the detector committed now — the one open, if any — owed its
/// response (`auto`).
fn commit(core: &mut Core, auto: bool) -> String {
    let turn = core
        .turn
        .take()
        .unwrap_or_else(|| OpenTurn::new(core.conversation.fresh_item_id(&core.ids)));
    let id = turn.item_id.clone();
    core.commit_turn(turn, vec![0; 2400], auto, None, None, None);
    id
}

/// The user started speaking again (the detector's `speech_started`).
fn speech(core: &mut Core) {
    core.on_judged(Detected {
        event: TurnEvent::SpeechStarted {
            audio_start_ms: 4000,
            onset_ms: 4300,
        },
        speech_end: None,
        at: tokio::time::Instant::now(),
        barge_in: false,
        end: None,
    });
}

/// `item_id`'s transcription failed with `e`.
fn failed(core: &mut Core, item_id: &str, e: GatewayError) {
    core.on_transcript(Done {
        item_id: item_id.into(),
        seconds: 0.1,
        result: Err(e),
        again: None,
        facts: Default::default(),
    });
}

/// A transcription that was attempted and failed.
fn engine_down() -> GatewayError {
    GatewayError::Upstream {
        status: 500,
        provider_type: None,
        message: "engine fell over".into(),
    }
}

/// `item_id`'s transcript came in.
fn transcribed(core: &mut Core, item_id: &str, text: &str) {
    core.on_transcript(Done {
        item_id: item_id.into(),
        seconds: 0.1,
        result: Ok(text.into()),
        again: None,
        facts: Default::default(),
    });
}

/// The server events the writer has sent so far, by type.
async fn sent(
    core: &mut Core,
    frames: &mut futures::channel::mpsc::UnboundedReceiver<axum::extract::ws::Message>,
) -> Vec<String> {
    sent_events(core, frames)
        .await
        .iter()
        .map(|v| v["type"].as_str().unwrap().to_string())
        .collect()
}

/// The server events the writer has sent so far.
async fn sent_events(
    core: &mut Core,
    frames: &mut futures::channel::mpsc::UnboundedReceiver<axum::extract::ws::Message>,
) -> Vec<Value> {
    core.flush().await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let mut out = Vec::new();
    while let Ok(m) = frames.try_recv() {
        if let axum::extract::ws::Message::Text(t) = m {
            out.push(serde_json::from_str(t.as_str()).unwrap());
        }
    }
    out
}

/// The `response.done` reason among `events`, if one ended.
fn done_reason(events: &[Value]) -> Option<String> {
    events
        .iter()
        .find(|e| e["type"] == "response.done")
        .map(|e| {
            e["response"]["status_details"]["reason"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
}

/// Whether a failed transcription was said among `events`, as with audio
/// input off (the session asked for no transcription events: `error`).
fn said_failed(events: &[Value]) -> bool {
    events.iter().any(|e| {
        e["type"] == "error" || e["type"] == "conversation.item.input_audio_transcription.failed"
    })
}

#[tokio::test]
async fn a_turn_in_the_hearing_has_words_until_it_is_transcribed() {
    let item = Item::Message(MessageItem {
        id: Some("item_1".into()),
        object: None,
        status: Some(ItemStatus::Completed),
        role: Role::User,
        content: vec![ContentPart::InputAudio {
            audio: None,
            transcript: None,
        }],
    });
    let mut hearing = Hearing::default();
    assert!(!has_words(&item, Some(&hearing)));
    assert!(!has_words(&item, None));
    hearing.insert("item_1".into(), Wav::build(vec![0; 240]));
    assert!(has_words(&item, Some(&hearing)));
    // Not a failed transcription either: a response may answer it.
    let mut conv = super::super::super::conversation::Conversation::default();
    conv.append(item);
    let awaited = ["item_1".to_string()];
    assert!(!transcripts_failed(&conv, &awaited, Some(&hearing)));
    assert!(transcripts_failed(&conv, &awaited, None));
}

#[tokio::test]
async fn an_audio_turn_launches_its_response_at_the_commit_and_noise_vetoes_it() {
    let (mut core, mut frames) = bound().await;
    let a = commit(&mut core, true);
    assert!(core.hears(&a));
    // Decided at the commit, launched at once, held.
    let active = core.active.as_ref().expect("launched at the commit");
    assert!(
        active.phase.launched(),
        "it does not wait for the transcript"
    );
    assert!(active.held.as_ref().is_some_and(|h| h.holding()));
    assert!(core.transcriber.busy() && !core.busy_for_launch());
    let ev = sent(&mut core, &mut frames).await;
    assert!(ev.contains(&"lmgw.chat.input".to_string()), "{ev:?}");
    // Its transcript: no words — the veto, a quiet cancel.
    transcribed(&mut core, &a, "");
    assert!(!core.hears(&a));
    assert!(core.active.is_none());
    let ev = sent(&mut core, &mut frames).await;
    assert!(ev.contains(&"response.done".to_string()), "{ev:?}");
    assert!(!ev.contains(&"error".to_string()), "{ev:?}");
}

#[tokio::test]
async fn a_due_response_to_turns_that_came_back_empty_is_dropped() {
    let (mut core, _frames) = bound().await;
    let a = commit(&mut core, true);
    transcribed(&mut core, &a, "Wie spät ist es?");
    let active = core.active.as_ref().unwrap();
    assert!(
        active.held.as_ref().is_some_and(|h| !h.holding()),
        "released"
    );
    // A cough while the answer runs: owed, and due after it.
    let b = commit(&mut core, true);
    let pending = format!("{:?}", core.pending);
    assert!(
        pending.contains("due: true"),
        "decided at the commit: {pending}"
    );
    // It came back empty: the debt goes, as noise's does.
    transcribed(&mut core, &b, "");
    assert!(core.pending.is_none());
    assert_eq!(core.active.as_ref().unwrap().output.gen, 1);
}

/// WP3 review #10: the launch itself — a turn transcribed before it goes
/// as its words, one still being transcribed as its WAV.
#[tokio::test]
async fn a_turn_transcribed_before_its_response_launched_goes_as_its_text() {
    let (mut core, _frames) = bound().await;
    // Not owed (create_response off): they wait for a response.create.
    let b = commit(&mut core, false);
    assert!(core.hears(&b));
    transcribed(&mut core, &b, "Zweite Frage.");
    assert!(!core.hears(&b), "its WAV is let go with its transcript");
    let c = commit(&mut core, false);
    assert!(core.hears(&c));
    let (_row, launch) = core
        .launch_heard(9, &[b.clone(), c.clone()], &[])
        .expect("an audio turn is among them");
    assert_eq!(launch.parts.len(), 2);
    assert!(
        matches!(&launch.parts[0], Spoken::Text(t) if t == "Zweite Frage."),
        "the transcribed turn goes as its words"
    );
    assert!(
        matches!(launch.parts[1], Spoken::Wav(_)),
        "the other as audio"
    );
    // Only transcribed turns: today's launch.
    transcribed(&mut core, &c, "Dritte Frage.");
    assert!(core.launch_heard(10, &[b, c], &[]).is_none());
}

/// WP3 review #3: a failed transcription of a turn the model heard — its
/// response is active, and its attempt carried the audio — plays the reply
/// and says nothing; before the attempt said it carried the audio, the
/// failure is said as with audio input off, and the response, answering no
/// words, ends quietly as `transcription_failed`.
#[tokio::test]
async fn a_failed_transcription_is_kept_only_for_a_turn_the_model_heard() {
    let (mut core, mut frames) = bound().await;
    let a = commit(&mut core, true);
    core.on_responder(1, Msg::Carried(true));
    sent(&mut core, &mut frames).await;
    failed(&mut core, &a, engine_down());
    let ev = sent_events(&mut core, &mut frames).await;
    assert!(!said_failed(&ev), "{ev:?}");
    let active = core.active.as_ref().expect("it plays");
    assert!(
        active.held.as_ref().is_some_and(|h| !h.holding()),
        "released"
    );
    assert!(core.bound.as_ref().unwrap().asr_errors.contains_key(&a));

    // Not known to have carried it yet: said, and ended quietly.
    let (mut core, mut frames) = bound().await;
    let a = commit(&mut core, true);
    sent(&mut core, &mut frames).await;
    failed(&mut core, &a, engine_down());
    let ev = sent_events(&mut core, &mut frames).await;
    assert!(said_failed(&ev), "{ev:?}");
    assert_eq!(done_reason(&ev).as_deref(), Some("transcription_failed"));
    assert!(core.active.is_none());
    assert!(
        core.bound.as_ref().unwrap().asr_errors.is_empty(),
        "no row says a model heard it"
    );
    // What the vetoed response still says is dropped, its end included —
    // which forgets it (WP3 review #9, #11).
    core.on_responder(
        1,
        Msg::Input {
            input: InputPath::Transcript,
            why: "late".into(),
        },
    );
    assert!(
        sent(&mut core, &mut frames).await.is_empty(),
        "no late note"
    );
    assert!(core.bound.as_ref().unwrap().unreleased.contains(&1));
    core.on_responder(
        1,
        Msg::Finished(Box::new(Err(crate::proxy::canceled("stopped")))),
    );
    assert!(core.bound.as_ref().unwrap().unreleased.is_empty());
}

/// WP3 review #2: a failure that is no transcription — no speech-to-text
/// model, the key's policy, the session's stop — is never heard, even
/// once the model answered from the audio.
#[tokio::test]
async fn a_transcription_never_attempted_is_never_heard() {
    let not_attempted = [
        GatewayError::InvalidRequest {
            code: "asr_not_configured",
            message: "no transcription model".into(),
        },
        GatewayError::KeyScope {
            key: "k".into(),
            alias: "hear".into(),
            reason: "not in its scope".into(),
        },
        crate::proxy::canceled("stopped by the caller"),
    ];
    for e in not_attempted {
        let (mut core, mut frames) = bound().await;
        let a = commit(&mut core, true);
        core.on_responder(1, Msg::Carried(true));
        sent(&mut core, &mut frames).await;
        failed(&mut core, &a, e);
        let ev = sent_events(&mut core, &mut frames).await;
        assert!(said_failed(&ev), "{ev:?}");
        assert_eq!(done_reason(&ev).as_deref(), Some("transcription_failed"));
        assert!(core.active.is_none());
    }
}

/// WP3 review #3 (a): the response a pause in mid-sentence cut is not
/// active, so its turn's failed transcription is said; the next response
/// counts that turn when its own came back empty — but only when the cut
/// response's attempt had carried it to the model (its row then says "not
/// transcribed", and the next request carries the placeholder).
#[tokio::test]
async fn a_failed_turn_owed_again_after_a_cut_counts_once_a_model_heard_it() {
    for carried in [true, false] {
        let (mut core, mut frames) = bound().await;
        let a = commit(&mut core, true);
        if carried {
            core.on_responder(1, Msg::Carried(true));
        }
        // The user goes on: the held response is cut, whatever
        // `interrupt_response` says (none here: push-to-talk's).
        speech(&mut core);
        assert!(core.active.is_none(), "cut unheard");
        let b = commit(&mut core, true);
        assert_eq!(core.active.as_ref().map(|a| a.output.gen), Some(2));
        sent(&mut core, &mut frames).await;
        failed(&mut core, &a, engine_down());
        let ev = sent_events(&mut core, &mut frames).await;
        assert!(said_failed(&ev), "the cut response is not active: {ev:?}");
        transcribed(&mut core, &b, "");
        let ev = sent_events(&mut core, &mut frames).await;
        if carried {
            let active = core.active.as_ref().expect("released");
            assert!(active.held.as_ref().is_some_and(|h| !h.holding()));
        } else {
            assert!(core.active.is_none(), "vetoed");
            assert_eq!(done_reason(&ev).as_deref(), Some("transcription_failed"));
        }
    }
}
