//! What the session says about the barge-in word check (realtime design
//! §6.4): the cut's start line names the words, and a backchannel is a
//! DEBUG line per check.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::realtime::audio_in::Detected;
use crate::realtime::protocol::{ContentPart, Item, ItemStatus, MessageItem, Role};
use crate::realtime::transcribe::{Again, CheckDone, Done};
use crate::realtime::turn::server_vad::TurnEvent;
use crate::realtime::voice::{SpeakVoice, VoiceOutcome, VoiceVia};

/// Everything logged at DEBUG and above on this thread while it lives.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Log {
    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        let log = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || log.clone())
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

#[tokio::test]
async fn the_cut_says_what_the_word_check_heard() {
    let log = Log::default();
    let _guard = log.capture();
    let mut r = Rig::new().await;
    r.core.speech.tts.alias = Some("say".into());
    r.core.speech.voice = VoiceOutcome::Resolved(SpeakVoice {
        send: Some("alba".into()),
        name: "alba".into(),
        via: VoiceVia::Model,
        verified: true,
    });
    r.core.session.output_modalities = Some(vec![Modality::Audio]);
    r.core.response_create(None, None);
    let at = Instant::now();
    r.core.on_judged_noted(
        Detected {
            event: TurnEvent::SpeechStarted {
                audio_start_ms: 4000,
                onset_ms: 4300,
            },
            speech_end: None,
            at,
            barge_in: true,
            end: None,
        },
        Some("words: \"Stopp.\" (12 ms for 0.40 s of speech)"),
        None,
    );
    let ev = r.events().await;
    assert_eq!(
        ev.last().unwrap()["response"]["status_details"]["reason"],
        "turn_detected"
    );
    let text = log.text();
    let line = text
        .lines()
        .find(|l| l.contains("speech from 4300 ms of the input (the barge-in gate)"))
        .unwrap_or_else(|| panic!("no start line in:\n{text}"));
    assert!(
        line.contains(
            "the response is cancelled (turn_detected) — words: \"Stopp.\" (12 ms for 0.40 s \
             of speech)"
        ),
        "{line}"
    );
    assert!(line.contains("INFO"), "{line}");
}

#[tokio::test]
async fn a_verdict_whose_turn_is_gone_says_so_and_decides_nothing() {
    // E5: it used to say "a backchannel, the answer plays on" — about a
    // turn that was no longer there.
    let log = Log::default();
    let _guard = log.capture();
    let mut r = Rig::new().await;
    r.core.on_word_check(CheckDone {
        id: 7,
        alias: "hear".into(),
        seconds: 0.54,
        took: Duration::from_millis(31),
        heard: Ok("Mhm.".into()),
    });
    assert!(r.events().await.is_empty());
    let text = log.text();
    let line = text
        .lines()
        .find(|l| l.contains("barge-in word check 7"))
        .unwrap_or_else(|| panic!("no line in:\n{text}"));
    assert!(
        line.contains(
            "heard \"Mhm.\" (31 ms for 0.54 s of speech), but its turn is gone (committed, \
             cleared, a normal turn or discarded since) — it decides nothing"
        ),
        "{line}"
    );
    assert!(line.contains("DEBUG"), "{line}");
    assert!(!text.contains("the answer plays on"), "{text}");
}

#[tokio::test]
async fn a_turn_transcribed_again_with_the_check_s_alias_says_so() {
    // N3: the turn's ASR heard nothing where the word check had heard words;
    // the second call's transcript is the turn's, and the log says whose.
    let log = Log::default();
    let _guard = log.capture();
    let mut r = Rig::new().await;
    for (failed, heard) in [
        (None, Ok("Wo ist der Bahnhof?".to_string())),
        (
            Some(crate::error::GatewayError::Upstream {
                status: 502,
                provider_type: None,
                message: "asr down".into(),
            }),
            Ok(String::new()),
        ),
    ] {
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
        r.core.transcriber.push(
            id.clone(),
            "hear".into(),
            Vec::new(),
            None,
            Some("checker".into()),
        );
        r.core.on_transcript(Done {
            item_id: id.clone(),
            seconds: 1.2,
            result: heard,
            again: Some(Again {
                asr: "hear".into(),
                check: "checker".into(),
                failed,
            }),
            facts: Default::default(),
        });
    }
    let text = log.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("came back empty from hear"))
        .collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(
        lines[0].contains(
            "while the barge-in word check's checker had heard words in it — transcribed again \
             with checker (one more ASR call, a usage row of its own): \"Wo ist der Bahnhof?\""
        ),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("transcribing it again with checker failed")
            && lines[1].contains("the turn stays empty"),
        "{}",
        lines[1]
    );
    assert!(lines.iter().all(|l| l.contains("INFO")), "{text}");
}
