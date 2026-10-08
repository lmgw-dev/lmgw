//! The phases of a response against the drained acknowledgement (realtime
//! design §4.3), driven directly: the test plays the responder, and decides
//! when the writer's acknowledgement reaches the core — the window WP3's
//! paced audio makes seconds long, and a text response makes too short to
//! hit over a socket.

use std::sync::Arc;

use axum::extract::ws::Message;
use serde_json::Value;
use tokio::sync::mpsc;

use super::super::asr::{AsrResolution, AsrVia};
use super::super::handshake::Limits;
use super::super::ids::Ids;
use super::super::protocol::Modality;
use super::super::resolve::{ChatResolution, Via};
use super::super::responder::{self, Msg};
use super::super::session::{Core, SessionInit};
use super::super::writer::{self, Drained};
use crate::error::GatewayError;
use crate::ir::{Completion, ContentPart, FinishReason, StreamDelta, Usage};

/// A marker generation no response has: what [`Rig::events`] waits for.
const SENTINEL: u64 = u64::MAX;

struct Rig {
    core: Core,
    frames: futures::channel::mpsc::UnboundedReceiver<Message>,
    drained: Drained,
    /// The drained acknowledgements the writer sent and the test has not
    /// handed to the core.
    acks: Vec<u64>,
    /// The model calls the core starts fail (there is no `chatty` here) and
    /// report here, unread: the test is the responder.
    _calls: mpsc::UnboundedReceiver<(u64, Msg)>,
}

impl Rig {
    async fn new() -> Self {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let (sink, frames) = futures::channel::mpsc::unbounded();
        let ids = Arc::new(Ids::new());
        let (out, drained, _writer) = writer::spawn(sink, ids.clone());
        let (responder_tx, calls): (responder::Tx, _) = mpsc::unbounded_channel();
        let (asr_tx, _) = mpsc::unbounded_channel();
        let (score_tx, _) = mpsc::unbounded_channel();
        let speech = super::super::session::speech::initial(&state)
            .await
            .unwrap();
        let init = SessionInit {
            running: None,
            state,
            ctx: Default::default(),
            requested_model: Some("chatty".into()),
            chat: ChatResolution {
                alias: Some("chatty".into()),
                via: Via::Alias,
            },
            asr: AsrResolution {
                alias: None,
                via: AsrVia::Default(None),
                missing: Some("none in this test".into()),
            },
            speech,
            slot: None,
            limits: Limits::from_settings(&Default::default()),
            bound: None,
        };
        let mut core = Core::new(init, ids, out, responder_tx, asr_tx, score_tx);
        core.session.output_modalities = Some(vec![Modality::Text]);
        Self {
            core,
            frames,
            drained,
            acks: Vec::new(),
            _calls: calls,
        }
    }

    /// Flush the core, and every event the writer has sent since the last
    /// call. Acknowledgements are collected in `acks`, not delivered.
    async fn events(&mut self) -> Vec<Value> {
        self.core.ob.drained(SENTINEL);
        self.core.flush().await;
        loop {
            match self.drained.recv().await.unwrap() {
                SENTINEL => break,
                g => self.acks.push(g),
            }
        }
        let mut out = Vec::new();
        while let Ok(m) = self.frames.try_recv() {
            if let Message::Text(t) = m {
                out.push(serde_json::from_str(t.as_str()).unwrap());
            }
        }
        out
    }

    fn delta(&mut self, gen: u64, text: &str) {
        let d = StreamDelta::TextDelta(text.into());
        self.core.on_responder(gen, Msg::Delta(d));
    }

    fn finish(&mut self, gen: u64, result: Result<Completion, GatewayError>) {
        self.core.on_responder(gen, Msg::Finished(Box::new(result)));
    }
}

fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

fn completion(text: &str) -> Completion {
    Completion {
        content: vec![ContentPart::Text { text: text.into() }],
        reasoning: String::new(),
        finish_reason: FinishReason::Stop,
        usage: Usage {
            prompt_tokens: Some(3),
            completion_tokens: Some(2),
            ..Default::default()
        },
        model: "m".into(),
        timings: None,
    }
}

#[tokio::test]
async fn response_done_waits_for_the_drained_ack_and_a_cancel_before_it_finishes_as_generated() {
    let mut r = Rig::new().await;
    r.core.response_create(Some("c1".into()), None);
    r.delta(1, "Hal");
    r.finish(1, Ok(completion("Hal")));
    let ev = r.events().await;
    assert_eq!(
        types(&ev),
        [
            "response.created",
            "response.output_item.added",
            "conversation.item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "conversation.item.done",
        ],
        "the output has left, and response.done waits for the core to hear so"
    );
    assert_eq!(r.acks, [1]);

    // Generation is over, in text mode: the client may have acted on the
    // items, so the cancel changes nothing — no error, no event.
    r.core.response_cancel(Some("x"), None);
    assert_eq!(r.events().await, Vec::<Value>::new());

    r.core.on_drained(1);
    let ev = r.events().await;
    assert_eq!(types(&ev), ["response.done"]);
    let done = &ev[0]["response"];
    assert_eq!(done["status"], "completed");
    assert_eq!(done["usage"]["total_tokens"], 5);
    assert_eq!(done["output"][0]["content"][0]["text"], "Hal");
}

#[tokio::test]
async fn a_cancel_while_a_failed_call_drains_cancels_and_a_create_then_is_queued() {
    let mut r = Rig::new().await;
    r.core.response_create(None, None);
    r.delta(1, "ab");
    r.core.on_responder(
        1,
        Msg::Delta(StreamDelta::Usage(Usage {
            prompt_tokens: Some(7),
            ..Default::default()
        })),
    );
    r.finish(1, Err(GatewayError::Transport("reset".into())));
    let ev = r.events().await;
    assert_eq!(types(&ev).last(), Some(&"response.output_text.delta"));
    assert_eq!(r.acks, [1]);

    // Draining: a create is queued behind it, not refused.
    r.core.response_create(Some("next".into()), None);
    assert_eq!(r.events().await, Vec::<Value>::new());

    r.core.response_cancel(None, None);
    let ev = r.events().await;
    assert_eq!(
        types(&ev),
        [
            "response.output_item.done",
            "conversation.item.done",
            "response.done",
            "response.created",
        ]
    );
    // Closed as far as it got, with no generation to purge it by.
    assert_eq!(ev[0]["item"]["status"], "incomplete");
    assert_eq!(ev[0]["item"]["content"][0]["text"], "ab");
    assert_eq!(ev[1]["item"], ev[0]["item"]);
    let done = &ev[2]["response"];
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["status_details"]["reason"], "client_cancelled");
    assert_eq!(
        done["usage"]["input_tokens"], 7,
        "what the upstream reported"
    );

    // The cancelled response's acknowledgement is stale now.
    r.core.on_drained(1);
    assert_eq!(r.events().await, Vec::<Value>::new());
    assert!(r.core.active.is_some(), "the queued response runs");
}

#[tokio::test]
async fn acks_and_cancels_are_generation_exact() {
    let mut r = Rig::new().await;
    r.core.response_create(None, None);
    r.delta(1, "one");
    r.finish(1, Ok(completion("one")));
    r.core.response_create(Some("two".into()), None);
    let first = r.events().await;
    r.core.on_drained(1);
    let ev = r.events().await;
    assert_eq!(types(&ev), ["response.done", "response.created"]);
    assert_eq!(ev[0]["response"]["status"], "completed");

    r.delta(2, "two");
    r.finish(2, Ok(completion("two")));
    r.events().await;
    // A repeated acknowledgement of the first response does not end the
    // second, which is draining on its own.
    r.core.on_drained(1);
    assert_eq!(r.events().await, Vec::<Value>::new());
    assert_eq!(r.acks, [1, 2]);
    r.core.on_drained(2);
    let ev = r.events().await;
    assert_eq!(types(&ev), ["response.done"]);
    assert_eq!(ev[0]["response"]["output"][0]["content"][0]["text"], "two");

    // A cancel of a third, mid-generation, touches nothing of the first two.
    r.core.response_create(None, None);
    r.delta(3, "thr");
    r.core.response_cancel(None, None);
    let ev = r.events().await;
    assert_eq!(ev.last().unwrap()["response"]["status"], "cancelled");
    let deltas: Vec<&str> = first
        .iter()
        .filter(|e| e["type"] == "response.output_text.delta")
        .map(|e| e["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, ["one"]);
    assert_eq!(r.acks, [1, 2]);
}

/// A user text item, appended as a committed turn would be; its id.
fn user_turn(r: &mut Rig, text: &str) -> String {
    use super::super::protocol::{ContentPart, Item, ItemStatus, MessageItem, Role};
    let id = r.core.conversation.fresh_item_id(&r.core.ids);
    r.core.conversation.append(Item::Message(MessageItem {
        id: Some(id.clone()),
        object: None,
        status: Some(ItemStatus::Completed),
        role: Role::User,
        content: vec![ContentPart::InputText { text: text.into() }],
    }));
    id
}

#[tokio::test]
async fn a_queued_create_that_cannot_start_errs_at_once_and_never_strands_an_owed_response() {
    use super::super::protocol::ResponseCreateParams;
    use super::super::voice::{SpeakVoice, VoiceOutcome, VoiceVia};
    let mut r = Rig::new().await;
    r.core.response_create(None, None);
    r.finish(1, Ok(completion("eins")));
    r.events().await;
    // A turn commits while it plays: its automatic response is due after it.
    let turn = user_turn(&mut r, "und weiter?");
    r.core.pending_commit(&turn, true, false);
    r.core.pending_resolve(&turn, false);

    // Out of band: refused now, echoing its id — not after response.done,
    // and it does not take the queue's place.
    let none: ResponseCreateParams =
        serde_json::from_value(serde_json::json!({"conversation": "none"})).unwrap();
    r.core.response_create(Some("oob".into()), Some(none));
    let ev = r.events().await;
    assert_eq!(types(&ev), ["error"]);
    assert_eq!(ev[0]["error"]["event_id"], "oob");

    // One that is fine when queued — a spoken answer, the voice resolved —
    // and not when it is due: the TTS alias went away meanwhile.
    let speech = r.core.speech.clone();
    r.core.speech.tts.alias = Some("say".into());
    r.core.speech.voice = VoiceOutcome::Resolved(SpeakVoice {
        send: Some("alba".into()),
        name: "alba".into(),
        via: VoiceVia::Model,
        verified: true,
    });
    let audio: ResponseCreateParams =
        serde_json::from_value(serde_json::json!({"output_modalities": ["audio"]})).unwrap();
    r.core.response_create(Some("spoken".into()), Some(audio));
    assert_eq!(r.events().await, Vec::<Value>::new(), "queued");
    r.core.speech = speech;

    r.core.on_drained(1);
    let ev = r.events().await;
    assert_eq!(
        types(&ev),
        ["response.done", "error", "response.created"],
        "{ev:?}"
    );
    assert_eq!(ev[1]["error"]["code"], "tts_not_configured");
    assert_eq!(ev[1]["error"]["event_id"], "spoken");
    // The owed response to the turn starts instead of waiting for good.
    assert!(r.core.active.is_some());
    assert!(r.core.pending.is_none());
}

#[tokio::test]
async fn a_detector_that_goes_down_mid_turn_closes_the_turn_and_decides_what_it_deferred() {
    // Package A review #5: the open turn's audio went with the detector,
    // but its item stayed the core's turn and the owed response it deferred
    // waited for a commit that never came.
    use super::super::audio_in::DetectorDown;
    use super::super::input::OpenTurn;
    let mut r = Rig::new().await;
    // A committed turn owes a response; new speech, announced, deferred it.
    let owed = user_turn(&mut r, "Wie spät ist es?");
    r.core.pending_commit(&owed, true, false);
    r.core.turn = Some(OpenTurn::new("item_next".into()));
    r.core.pending_defer();

    r.core.detector_down(DetectorDown {
        why: "Silero failed".into(),
        at_ms: 4200,
    });
    let ev = r.events().await;
    assert_eq!(
        types(&ev),
        [
            "input_audio_buffer.speech_stopped",
            "error",
            "response.created"
        ],
        "{ev:?}"
    );
    assert_eq!(ev[0]["item_id"], "item_next");
    assert_eq!(ev[0]["audio_end_ms"], 4200);
    assert_eq!(ev[1]["error"]["code"], "turn_detection_unavailable");
    assert!(r.core.turn.is_none());
    assert!(r.core.pending.is_none());
    assert!(r.core.active.is_some(), "the owed response started");

    // With no turn open there is nothing to close: the error alone.
    r.core.detector_down(DetectorDown {
        why: "Silero failed".into(),
        at_ms: 5000,
    });
    let ev = r.events().await;
    assert_eq!(types(&ev).last(), Some(&"error"));
    assert!(!types(&ev).contains(&"input_audio_buffer.speech_stopped"));
    assert!(r.core.turn.is_none());
}

#[tokio::test]
async fn the_timing_line_has_what_the_core_and_the_responder_saw() {
    use super::super::protocol::ResponseStatus;
    use super::super::responder::Mark;
    let mut r = Rig::new().await;
    r.core.response_create(None, None);
    r.core.on_responder(1, Msg::Mark(Mark::FirstToken));
    r.core.on_responder(1, Msg::Mark(Mark::FirstClause));
    r.delta(1, "Hal");
    let active = r.core.active.as_ref().unwrap();
    let line = active
        .timing
        .line(ResponseStatus::Completed, std::time::Instant::now());
    assert!(line.starts_with("completed — LLM first token "), "{line}");
    assert!(line.contains(", first clause "), "{line}");
    assert!(line.contains("; first text at "), "{line}");
    assert!(line.ends_with(" ms from response.created"), "{line}");
    assert!(!line.contains("end of turn"), "no turn: {line}");
}

#[tokio::test]
async fn a_response_is_timed_from_its_own_turn_only() {
    // Package A review #7: a turn no response answered stayed the next
    // response's, whose line then ran from that turn's end of speech.
    use super::super::protocol::ResponseStatus;
    use super::TurnTiming;
    use std::time::{Duration, Instant};
    let mut r = Rig::new().await;
    let turn = |id: &str| TurnTiming {
        item_id: id.into(),
        speech_end: Some(Instant::now() - Duration::from_secs(30)),
        committed: Instant::now() - Duration::from_secs(29),
        transcribed: None,
        ended_by: None,
    };
    // A committed turn that was noise: no response is its.
    let noise = user_turn(&mut r, "  ");
    r.core.last_turn = Some(turn(&noise));
    r.core.pending_commit(&noise, true, false);
    r.core.pending_resolve(&noise, false);
    assert!(r.core.pending.is_none(), "no response for noise");
    assert!(
        r.core.last_turn.is_none(),
        "and no response is timed from it"
    );

    // A typed response later is timed from its own response.created.
    user_turn(&mut r, "Hallo");
    r.core.response_create(None, None);
    let active = r.core.active.as_ref().unwrap();
    assert!(active.timing.turn.is_none());
    let line = active
        .timing
        .line(ResponseStatus::Completed, Instant::now());
    assert!(line.ends_with(" ms from response.created"), "{line}");

    // At its launch a response renders every committed turn: the latest,
    // committed after it was created, is its own.
    r.core.last_turn = Some(turn("item_later"));
    r.core.timing_launched();
    let active = r.core.active.as_ref().unwrap();
    assert_eq!(
        active.timing.turn.as_ref().map(|t| t.item_id.as_str()),
        Some("item_later")
    );
    assert!(r.core.last_turn.is_none());
}

#[tokio::test]
async fn a_turn_without_words_that_nothing_is_owed_to_times_nothing() {
    // A2 review 5: with create_response off, or a client's commit, no
    // decision about a debt drops a noise turn — its transcript does.
    use super::super::protocol::{ContentPart, Item, ItemStatus, MessageItem, Role};
    use super::super::transcribe::Done;
    use super::TurnTiming;
    use std::time::Instant;
    let mut r = Rig::new().await;
    let audio_turn = |r: &mut Rig| {
        let id = r.core.conversation.fresh_item_id(&r.core.ids);
        r.core.conversation.append(Item::Message(MessageItem {
            id: Some(id.clone()),
            object: None,
            status: Some(ItemStatus::Completed),
            role: Role::User,
            content: vec![ContentPart::InputAudio {
                audio: None,
                transcript: None,
            }],
        }));
        id
    };
    for (heard, kept) in [
        (Ok("  ".to_string()), false),
        (
            Err(GatewayError::Upstream {
                status: 502,
                provider_type: None,
                message: "asr down".into(),
            }),
            false,
        ),
        (Ok("Hallo".to_string()), true),
    ] {
        let id = audio_turn(&mut r);
        r.core.last_turn = Some(TurnTiming {
            item_id: id.clone(),
            speech_end: None,
            committed: Instant::now(),
            transcribed: None,
            ended_by: None,
        });
        // Not owed (create_response off): `pending_commit(.., false)`.
        r.core.pending_commit(&id, false, false);
        r.core
            .transcriber
            .push(id.clone(), "asr".into(), Vec::new(), None, None);
        r.core.on_transcript(Done {
            item_id: id.clone(),
            seconds: 0.5,
            result: heard,
            again: None,
            facts: Default::default(),
        });
        assert_eq!(r.core.last_turn.is_some(), kept, "{id}");
    }
}

mod barge;
mod heard;
mod hearing;
mod held;
mod hold;
mod window;
mod words;
