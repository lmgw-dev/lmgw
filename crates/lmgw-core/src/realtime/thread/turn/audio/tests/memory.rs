//! The session's refusal memory (§3.5, WP2 review #3, #4): kept by the
//! model that refused, read by the verdict's session row for exactly the
//! models a turn may reach; a note only when no retry said why; lmgw's own
//! refusals never kept.

use std::sync::Arc;

use super::super::*;
use crate::state::AppState;
use crate::store::InputPath;
use crate::web::chat_voice::bound::AudioInput;

/// A core bound to a thread of its own.
async fn bound_core() -> crate::realtime::session::Core {
    use crate::realtime::asr::{AsrResolution, AsrVia};
    use crate::realtime::resolve::{ChatResolution, Via};
    let state = AppState::init_for_tests().await.unwrap();
    let thread = crate::store::ChatThread {
        id: 4,
        model_alias: "m".into(),
        ..Default::default()
    };
    let bind = state.chat_live.bind_voice(thread.id, "the dashboard");
    let binding = crate::realtime::thread::Binding {
        thread_id: thread.id,
        title: "t".into(),
        temporary: false,
        admin_tools: false,
        cfg: bound::voice(&state.snapshot(), &thread),
        audio_input: bound::audio_input(&state, &thread).await,
        warm: vec![],
        guard: Some(bind.guard),
        taken: bind.taken,
        taken_by: bind.taken_by,
        fence: bind.fence,
        tasks: Default::default(),
    };
    let (sink, _frames) = futures::channel::mpsc::unbounded();
    let ids = Arc::new(crate::realtime::ids::Ids::new());
    let (out, _drained, _writer) = crate::realtime::writer::spawn(sink, ids.clone());
    let (responder_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (asr_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (score_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let speech = crate::realtime::session::speech::initial(&state)
        .await
        .unwrap();
    let init = crate::realtime::session::SessionInit {
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
    crate::realtime::session::Core::new(init, ids, out, responder_tx, asr_tx, score_tx)
}

fn hears(model: &str, via: &[&str]) -> AudioInput {
    AudioInput {
        path: InputPath::Audio,
        model: model.into(),
        why: None,
        lead: None,
        blocked: None,
        via: via.iter().map(|m| m.to_string()).collect(),
        lacks: false,
    }
}

#[tokio::test]
async fn a_refusal_is_kept_by_the_model_that_refused() {
    let mut core = bound_core().await;
    let note = |why: &str| Msg::Input {
        input: InputPath::Transcript,
        why: why.into(),
    };
    let kept = |model: &str, why: &str| Msg::Refused {
        refused: Refused {
            model: model.into(),
            why: why.into(),
        },
        note: None,
    };
    // Notes alone keep nothing: lmgw's own refusals, a skipped attempt.
    core.on_responder(
        1,
        note("gpt does not take audio input, so nothing was sent"),
    );
    core.on_responder(2, note("audio input is off (this thread)"));
    assert!(core.bound.as_ref().unwrap().refused.is_empty());
    core.on_responder(
        3,
        kept("gemma", "it refused the audio this session: no audio here"),
    );
    let b = core.bound.as_mut().unwrap();
    assert_eq!(b.refused.len(), 1);

    // The thread's model that refused: its later turns go as text.
    b.audio_input.verdict = hears("gemma", &["gemma"]);
    let now = core.audio_now().unwrap();
    assert_eq!(now.path, InputPath::Transcript);
    assert_eq!(
        now.why.as_deref(),
        Some("it refused the audio this session: no audio here")
    );
    // The thread switched to another model: it hears.
    core.bound.as_mut().unwrap().audio_input.verdict = hears("other", &["other"]);
    assert!(core.hears_next());
    // A candidate alias hears while it may still pick a candidate that did
    // not refuse, and reads the transcript once every one did.
    core.bound.as_mut().unwrap().audio_input.verdict = hears("pick", &["gemma", "gemma-b"]);
    assert!(core.hears_next());
    core.on_responder(
        4,
        kept(
            "gemma-b",
            "its server failed on the audio this session: reset",
        ),
    );
    assert!(!core.hears_next());
    // A transcript verdict keeps its own why.
    let off = AudioInput {
        path: InputPath::Transcript,
        why: Some("audio input is off (this thread)".into()),
        ..hears("gemma", &[])
    };
    core.bound.as_mut().unwrap().audio_input.verdict = off.clone();
    assert_eq!(core.audio_now().unwrap(), off);
}
