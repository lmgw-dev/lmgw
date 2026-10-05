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
//!
//! The callers are the stored reply's `speak` route (`super::speak`) and
//! the speech tee of a turn sent with `speak: true` (`super::tee`).

mod clip;
mod lead;
mod out;
mod plan;
mod run;

pub(crate) use lead::lead;
pub(crate) use out::SpeechRx;
pub(crate) use plan::{plan, resolve_shown, style_of, thread_seed, voice_of, Plan, Refusal};
pub(crate) use run::{cold, Feed};

use std::time::Instant;

use tokio::sync::{mpsc, oneshot};

use crate::realtime::warm::{warm_group, Reporter, WarmMode};
use crate::state::SharedState;
use crate::store::ChatThread;

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
/// the turn's language sentence says the reply is heard only then.
pub(crate) fn start(
    state: &SharedState,
    repo: ChatRepo,
    thread: &ChatThread,
    (started, planned): (Instant, Option<oneshot::Sender<bool>>),
    warm: bool,
    feed: mpsc::UnboundedReceiver<Feed>,
) -> SpeechRx {
    let (registered, stop) = state.chat_live.speaking(thread.id);
    let (out, reader) = out::channel();
    let (state, thread) = (state.clone(), thread.clone());
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
        };
        run::run(state, run, feed, states, out).await;
    });
    reader
}
