//! Speech for the Chat (chat-voice design §6): realtime's speech pipeline —
//! clause cutting, speakable text, delivery cues, voice resolution and
//! per-clause synthesis — driven without a writer, for read-aloud.
//!
//! - [`plan`]: how a thread speaks, its seed drawn on first use.
//! - [`clip`]: a voice clip its TTS cannot clone without a transcript,
//!   refused before anything starts.
//! - [`run`]: one read-aloud, fed text, answering the Chat's speech frames.
//! - [`out`]: those frames on their way to the page — the audio raw until
//!   the page takes it, a page that falls behind playback said.
//! - [`lead`]: where a continued reply's read-aloud starts.
//! - [`start`]: one read-aloud in its own task.
//! - [`speak_planned`]: one speech of a plan made elsewhere, in its own
//!   task, registered with no thread.
//!
//! The callers are the stored reply's `speak` route (`super::speak`), the
//! speech tee of a turn sent with `speak: true` (`super::tee`), and the
//! profile editor's Speak (`chat_profiles::try`), which plans with its
//! unsaved draft.

mod clip;
mod lead;
mod out;
mod plan;
mod run;

pub(crate) use lead::lead;
pub(crate) use out::SpeechRx;
pub(crate) use plan::{
    plan, plan_with, resolve_shown, style_of, thread_seed, voice_of, Plan, Refusal,
};
pub(crate) use run::{cold, Feed};

use std::time::Instant;

use lmgw_api_types::chat_frames as frames;
use tokio::sync::{mpsc, oneshot};

use crate::realtime::warm::{warm_group, Reporter, WarmMode};
use crate::state::SharedState;
use crate::store::ChatThread;
use crate::telemetry::RequestClass::Audio;

use super::super::chat_caller::Caller;
use super::super::chat_repo::ChatRepo;

/// Start one read-aloud of `thread` (§6.3, §6.4), fed by `feed`; its frames
/// come out of the returned reader. It is registered with the thread's
/// speech at once, so `speech/stop` reaches it from now on, and for as long
/// as it speaks: `speech/stop` counts the read-alouds still speaking (review
/// n3).
///
/// Its own task, so it writes its row however it ends, and so the caller's
/// first byte waits on none of it (review m4): it plans the speech — the
/// voice facts, the seed's first draw — and a refusal is its one
/// `speech_error` frame; it warms the TTS in the Background when `warm` (a
/// turn read as it streams: beside the prefill, never evicting), unless the
/// speech was stopped or its reader went away first (review n9); and it
/// speaks ([`run::run`]). Dropping the reader stops it. `planned` (a turn
/// read as it streams) is told whether the plan stands, once it is made:
/// the turn's language sentence says the reply is heard only then. Its TTS
/// calls are `caller`'s: checked against a device's key and charged to it
/// (client-apps design L4).
pub(crate) fn start(
    state: &SharedState,
    caller: &Caller,
    repo: ChatRepo,
    thread: &ChatThread,
    (started, planned): (Instant, Option<oneshot::Sender<bool>>),
    warm: bool,
    feed: mpsc::UnboundedReceiver<Feed>,
) -> SpeechRx {
    let (registered, stop) = state.chat_live.speaking(thread.id);
    let (out, reader) = out::channel();
    let (state, thread, ctx) = (state.clone(), thread.clone(), caller.ctx());
    let charged = caller.charged();
    tokio::spawn(async move {
        let _registered = registered;
        let plan = plan::plan(&state, repo, &thread).await;
        if let Some(planned) = planned {
            let _ = planned.send(plan.is_ok());
        }
        let plan = match plan {
            Ok(plan) => plan,
            Err(refusal) => return out.frame(run::refused(&refusal)),
        };
        // A device's read-aloud warms and speaks only with a TTS its key may
        // use (client-apps design §1.3, review W2-3): scope and budget before
        // the warm below can load it; the speaker counts each call.
        if let Err(e) = device_check(&state, charged.as_ref(), &plan).await {
            let error = frames::SpeechError {
                code: e.code().to_string(),
                message: e.to_string(),
            };
            return out.frame(super::super::chat_turn::TurnFrame::of(
                "speech_error",
                &error,
            ));
        }
        let states = (warm && !stop.is_raised() && !out.is_closed()).then(|| {
            let (warmed, states) = mpsc::unbounded_channel();
            let (stage, label, st) = (plan.warm(), plan.speech.label.clone(), state.clone());
            tokio::spawn(async move {
                let report = Reporter::to(warmed);
                warm_group(&st, &label, WarmMode::Background, &[stage], &report).await;
            });
            states
        });
        let run = run::Run {
            plan,
            stop,
            started,
            ctx,
        };
        run::run(state, run, feed, states, out).await;
    });
    reader
}

/// A device's check of the TTS `plan` speaks with (`charged`: its context;
/// `None` for the owner, who is not checked): its key's scope and budget,
/// not counted — the speaker counts each call. A refusal has written its
/// row.
pub(crate) async fn device_check(
    state: &SharedState,
    charged: Option<&crate::proxy::RequestCtx>,
    plan: &Plan,
) -> Result<(), crate::error::GatewayError> {
    let Some(ctx) = charged else {
        return Ok(());
    };
    let alias = plan.speech.alias.as_str();
    crate::proxy::policy_checked(state, plan.speech.proto, ctx, alias, Audio).await
}

/// Speak `text` with `plan` (the profile editor's Speak, personality-profiles
/// design §3.1): the read-aloud's pipeline — clauses, the speakable pass,
/// cues, the plan's TTS, voice, style and seed — in its own task, its frames
/// out of the returned reader, as [`start`]'s. It is registered with no
/// thread's speech, and nothing warms ahead of it; `stop` (or the reader
/// going away) stops it. Its TTS calls are `caller`'s, charged to a device's
/// key — which the caller has checked first ([`device_check`]); it writes
/// one TTS row.
pub(crate) fn speak_planned(
    state: &SharedState,
    caller: &Caller,
    plan: Plan,
    (started, stop): (Instant, crate::proxy::StopSignal),
    text: String,
) -> SpeechRx {
    let (out, reader) = out::channel();
    let (state, ctx) = (state.clone(), caller.ctx());
    tokio::spawn(async move {
        let (feed, fed) = mpsc::unbounded_channel();
        let _ = feed.send(Feed::Text(text));
        drop(feed);
        let run = run::Run {
            plan,
            stop,
            started,
            ctx,
        };
        run::run(state, run, fed, None, out).await;
    });
    reader
}
