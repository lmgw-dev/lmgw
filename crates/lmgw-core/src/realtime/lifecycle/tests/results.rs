//! The continuation's rule (MCP Tasks design §3.4, `lifecycle::bound`): a
//! response the client asked for with no new words, and with no turn the
//! client committed before it, runs while a job result is owed, and is
//! `empty_turn` with none; an automatic response — turn detection's, owed
//! to a turn without words — stays `empty_turn` whatever is owed: the
//! client picks the moment a result is spoken. Whether a create may
//! continue is decided when it comes, and kept through the turns it picks
//! up (review finding 1): a cough it was carried over, a cough that cut it
//! before anyone heard it.

use serde_json::Value;

use super::hearing::{bound, commit, sent_events, speech, transcribed};
use crate::realtime::session::Core;
use crate::realtime::thread::tasks::Owed;
use crate::store::{AudioInputMode, InputPath};

type Frames = futures::channel::mpsc::UnboundedReceiver<axum::extract::ws::Message>;

/// A core bound to a thread whose turns go to the model as their
/// transcript (audio input off), owing job result 12.
async fn owing() -> (Core, Frames) {
    let (mut core, frames) = bound().await;
    let b = core.bound.as_mut().unwrap();
    b.audio_input.value = AudioInputMode::Off;
    b.audio_input.verdict.path = InputPath::Transcript;
    b.tasks = Owed::owing_for_tests(&[12]);
    (core, frames)
}

/// The `error` codes among `events`, a failed `response.done`'s included.
fn refusals(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e["type"].as_str() {
            Some("error") => e["error"]["code"].as_str(),
            Some("response.done") => e["response"]["status_details"]["error"]["code"].as_str(),
            _ => None,
        })
        .map(str::to_string)
        .collect()
}

/// The active response launched as a continuation: past the refusal, its
/// call started, decided as the client's create.
fn continues(core: &Core) -> bool {
    core.active
        .as_ref()
        .is_some_and(|a| a.phase.launched() && a.continuation)
}

#[tokio::test]
async fn only_a_client_s_bare_create_is_a_continuation() {
    let (mut core, _frames) = bound().await;
    // Nothing owed: a bare create answers nothing.
    let refused = core.bound_refusal(&[], true).expect("empty_turn");
    assert_eq!(refused.code.as_deref(), Some("empty_turn"));

    core.bound.as_mut().unwrap().tasks = Owed::owing_for_tests(&[12]);
    assert!(
        core.bound_refusal(&[], true).is_none(),
        "the client's bare create answers the result"
    );
    // The automatic response for a turn that came without words (a cough
    // the detector committed): still nothing to answer.
    let refused = core
        .bound_refusal(&["item_cough".to_string()], false)
        .expect("empty_turn");
    assert_eq!(refused.code.as_deref(), Some("empty_turn"));
    // A client's create after a commit of its own answers that commit.
    let refused = core.bound_refusal(&[], false).expect("empty_turn");
    assert_eq!(refused.code.as_deref(), Some("empty_turn"));
}

/// A cough cuts the continuation before anyone heard it: its create is
/// held again, picks up the cough's turn, and still runs as a continuation
/// once the cough came back without words.
#[tokio::test]
async fn a_cough_that_cut_a_continuation_nobody_heard_runs_it_again() {
    let (mut core, mut frames) = owing().await;
    core.response_create(None, None);
    assert!(continues(&core), "the bare create continues");
    sent_events(&mut core, &mut frames).await;

    speech(&mut core);
    let ev = sent_events(&mut core, &mut frames).await;
    let done = ev.iter().find(|e| e["type"] == "response.done").unwrap();
    assert_eq!(
        done["response"]["status_details"]["reason"],
        "turn_detected"
    );
    assert!(core.active.is_none());
    assert!(core.pending_carries(), "its create is held for the turn");

    let cough = commit(&mut core, true);
    transcribed(&mut core, &cough, "");
    let ev = sent_events(&mut core, &mut frames).await;
    assert!(refusals(&ev).is_empty(), "{ev:#?}");
    assert!(continues(&core), "the cut continuation runs again: {ev:#?}");
    let a = core.active.as_ref().unwrap();
    assert_eq!(a.answers, [cough], "it answers the cough's turn too");
}

/// The client's bare create while the user coughs is held for the turn,
/// and answers the results once the cough came back without words.
#[tokio::test]
async fn a_bare_create_carried_over_a_cough_continues() {
    let (mut core, mut frames) = owing().await;
    speech(&mut core);
    core.response_create(None, None);
    assert!(core.active.is_none());
    assert!(core.pending_carries());
    let cough = commit(&mut core, true);
    transcribed(&mut core, &cough, "");
    let ev = sent_events(&mut core, &mut frames).await;
    assert!(refusals(&ev).is_empty(), "{ev:#?}");
    assert!(continues(&core), "{ev:#?}");
}

/// Push-to-talk: a commit of silence and its `response.create` are a
/// cough's equal — `empty_turn`, no result spoken (K26); the next bare
/// create, with no commit before it, continues. The same while the
/// commit's transcript is still being made: refused once it is in.
#[tokio::test]
async fn a_silent_push_to_talk_commit_and_its_create_speak_no_result() {
    let (mut core, mut frames) = owing().await;
    let silence = commit(&mut core, false);
    transcribed(&mut core, &silence, "");
    core.response_create(None, None);
    let ev = sent_events(&mut core, &mut frames).await;
    assert_eq!(refusals(&ev), ["empty_turn"], "{ev:#?}");
    assert!(core.active.is_none());

    core.response_create(None, None);
    let ev = sent_events(&mut core, &mut frames).await;
    assert!(refusals(&ev).is_empty(), "{ev:#?}");
    assert!(continues(&core), "no commit before it: {ev:#?}");

    // Its transcript not in yet when the create comes.
    let (mut core, mut frames) = owing().await;
    let silence = commit(&mut core, false);
    core.response_create(None, None);
    assert!(core.active.as_ref().is_some_and(|a| !a.continuation));
    transcribed(&mut core, &silence, "");
    // Refused at its launch: it ends `failed` with `empty_turn` once the
    // writer drained it (the test holds no drained acknowledgement).
    let ended = core.active.as_ref().and_then(|a| a.ended.as_ref());
    let code = match ended {
        Some(Err(f)) => f.error.code.clone(),
        _ => None,
    };
    assert_eq!(code.as_deref(), Some("empty_turn"));
    let ev = sent_events(&mut core, &mut frames).await;
    assert!(
        !ev.iter()
            .any(|e| e["type"] == "lmgw.chat.input" || e["type"] == "response.output_item.added"),
        "{ev:#?}"
    );
}
