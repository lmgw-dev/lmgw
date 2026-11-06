//! A chat turn's `state` frames (chat-voice design §4.3).
//!
//! Before an admission whose model is not up and ready (absent, starting
//! for another caller, or on its way out — any of them is a wait), the
//! turn says `loading` for its `chat` stage; once admission settles it
//! says `ready` with the time it took — or `fallback` when the outside-VRAM
//! verdict answered with the fallback, `held` or `failed` when admission
//! refused (the turn's own `error` frame follows, as before). A model that
//! is up, a route the GPU hold already swapped to its fallback and a cloud
//! route say nothing. A candidate alias says nothing up front either (its
//! pick is made at admission, and may be a model that is up), only a
//! download its pick waits for, and what follows it.
//!
//! The GPU hold and a benchmark's lease refuse a local model with no usable
//! fallback earlier, at the route's resolve, before any admission: that
//! refusal says `held` too ([`held_at_resolve`], WP11 server review M1), so
//! a hold is the amber chip §4.3 promises on every path, not an error.
//!
//! The plain text `send` carries these frames too: a wire change the page
//! before it ignores (it skips unknown events).
//!
//! A tool loop says the same when its claim, let go for a picture, has to
//! load the chat model again ([`RegainFrames`], chat-media-tools design
//! §5.3): `loading`, then `ready` with the time — or the refusal's
//! `held`/`failed`.
//!
//! An admission that first downloads the model's engine image (machine-check
//! design §5) says so: the turn follows its own wait ([`stage_watch::follow`])
//! and each step of the download is a `downloading` frame with its bytes;
//! the phase the download's end sends (`starting`, also with admission
//! switched off) says `loading` again, and the `ready` time counts from
//! there.
//!
//! **The client never holds up an admission.** The turn's channel is
//! bounded, and a client that stopped reading (an app sent to the
//! background with its socket open) fills it; an admission waiting for that
//! room would hold the admission gate it took meanwhile, and every other
//! local start would wait behind a phone in a pocket. So while the
//! admission runs, the frames it causes go through one slot: the newest
//! frame waits there for room, beside the admission, and a newer one takes
//! its place. What admission settled on (`ready`, `fallback`, `held`,
//! `failed`) is said after it returned, and is waited for as before.
//!
//! [`stage_watch::follow`]: crate::vram::stage_watch::follow

use std::future::Future;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::error::GatewayError;
use crate::gate::{OpenFailed, Opened, Routed};
use crate::realtime::warm::{ModelState, WarmOutcome};
use crate::state::SharedState;

use super::super::chat_turn::{Events, TurnFrame};

/// Admit `routed` — the turn's model, asked for as `alias` — saying its
/// `state` around an admission that has to start it (module doc).
pub(crate) async fn admit_reporting(
    state: &SharedState,
    routed: Routed,
    alias: &str,
    tx: &Events,
) -> Result<Opened, OpenFailed> {
    if routed.headers().fallback().is_some() {
        return routed.admit(state).await;
    }
    // A candidate alias's pick is admission's: only a download it waits
    // for is said, never a `loading` up front for a model that is up.
    let candidate = state.snapshot().candidate_alias(alias).is_some();
    let cold = !candidate
        && crate::vram::classify(routed.resolved())
            .is_some_and(|t| state.runtime().ready_port(t.class, &t.model_id).is_none());
    if !candidate && !cold {
        return routed.admit(state).await;
    }
    if cold {
        frame(tx, ModelState::loading("chat", alias)).await;
    }
    let (admitted, said) = downloads_said(tx, alias, routed.admit(state)).await;
    if candidate && !said.downloaded {
        return admitted;
    }
    let outcome = match &admitted {
        Ok(o) => match o.headers.fallback() {
            Some(fb) => WarmOutcome::Fallback {
                answered_by: fb.to_string(),
            },
            None => WarmOutcome::Ready {
                ms: said.loading.map(|t| t.elapsed().as_millis() as u64),
            },
        },
        Err(f) => WarmOutcome::refused(&f.error),
    };
    if let Some(s) = ModelState::of("chat", alias, &outcome) {
        frame(tx, s).await;
    }
    admitted
}

/// What [`downloads_said`] said while the admission ran.
#[derive(Debug, Clone, Copy)]
struct Said {
    /// When the load began: when the admission began, or at the phase after
    /// a download; `None` when a download was the last thing heard, so its
    /// end is not known.
    loading: Option<Instant>,
    /// A `downloading` frame was due: the admission waited for an engine
    /// download.
    downloaded: bool,
}

/// Run `admission`, saying each step of an engine download it waits for
/// as a `downloading` frame, and `loading` again at the phase after it
/// (module doc). The frames wait for room in one slot beside the
/// admission, never in front of it: the newest replaces one that is still
/// waiting, and one still waiting for room when the admission returns is
/// dropped, since what the admission settled on is said next.
async fn downloads_said<F: Future>(tx: &Events, alias: &str, admission: F) -> (F::Output, Said) {
    let (phases, mut heard) = mpsc::unbounded_channel();
    // Boxed: the admission's future is the turn chain's largest, and it
    // must not grow the poll frames the turn's stack holds.
    let mut admission = Box::pin(crate::vram::stage_watch::follow(phases, admission));
    let mut said = Said {
        loading: Some(Instant::now()),
        downloaded: false,
    };
    let mut pending: Option<ModelState> = None;
    loop {
        // In this order: a frame with room goes out, and a phase heard is
        // taken, before the admission's end is — so a reader that keeps up
        // hears every step, in order, before what admission settled on.
        tokio::select! {
            biased;
            Ok(slot) = tx.reserve(), if pending.is_some() => {
                if let Some(s) = pending.take() {
                    slot.send(TurnFrame::of("state", &s));
                }
            }
            Some(phase) = heard.recv() => match ModelState::downloading("chat", alias, &phase) {
                Some(s) => {
                    said.loading = None;
                    said.downloaded = true;
                    pending = Some(s);
                }
                None if said.loading.is_none() => {
                    said.loading = Some(Instant::now());
                    pending = Some(ModelState::loading("chat", alias));
                }
                None => {}
            },
            out = &mut admission => return (out, said),
        }
    }
}

/// The `held` frame of a refusal at the route's resolve: said for the GPU
/// hold and a benchmark's lease (module doc), nothing for any other
/// refusal. The turn's `error` frame follows.
pub(crate) async fn held_at_resolve(tx: &Events, alias: &str, e: &GatewayError) {
    if !matches!(
        e,
        GatewayError::GpuHold { .. } | GatewayError::GpuBenchmark { .. }
    ) {
        return;
    }
    if let Some(s) = ModelState::of("chat", alias, &WarmOutcome::refused(e)) {
        frame(tx, s).await;
    }
}

/// The `state` frames of a tool loop's regain (module doc).
pub(crate) struct RegainFrames {
    tx: Events,
}

impl RegainFrames {
    pub fn new(tx: Events) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { tx })
    }
}

impl crate::agent::claim::RegainReport for RegainFrames {
    fn loading<'a>(&'a self, alias: &'a str) -> crate::erased::Erased<'a, ()> {
        Box::pin(frame(&self.tx, ModelState::loading("chat", alias)))
    }

    fn settled<'a>(
        &'a self,
        alias: &'a str,
        outcome: Result<u64, &'a GatewayError>,
    ) -> crate::erased::Erased<'a, ()> {
        Box::pin(async move {
            let outcome = match outcome {
                Ok(ms) => WarmOutcome::Ready { ms: Some(ms) },
                Err(e) => WarmOutcome::refused(e),
            };
            if let Some(s) = ModelState::of("chat", alias, &outcome) {
                frame(&self.tx, s).await;
            }
        })
    }
}

async fn frame(tx: &Events, s: ModelState) {
    let _ = tx.send(TurnFrame::of("state", &s)).await;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::vram::stage_watch::{tell, WaitPhase};

    fn dl(done: u64) -> WaitPhase {
        WaitPhase::Downloading {
            image: "registry.example/engine:cuda".into(),
            done_bytes: done,
            total_bytes: Some(10),
            size_note: None,
        }
    }

    /// A turn's channel of one slot that its client never reads: full.
    fn stalled() -> (Events, mpsc::Receiver<TurnFrame>) {
        let (out, rx) = mpsc::channel(1);
        let tx = Events::new(out, Default::default());
        tx.try_send(TurnFrame::of("state", &ModelState::loading("chat", "c")))
            .unwrap();
        (tx, rx)
    }

    fn state_of(f: &TurnFrame) -> serde_json::Value {
        assert_eq!(f.event, "state");
        serde_json::from_str(&f.data).unwrap()
    }

    /// A client that stopped reading holds up no admission (module doc,
    /// review H1): with the turn's channel full, the admission runs through
    /// its download and the gate it takes after it, saying a phase while it
    /// holds the gate, and another admission waiting for that gate gets it.
    #[tokio::test]
    async fn a_client_that_stopped_reading_holds_up_no_admission() {
        let (tx, mut rx) = stalled();
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let ours = gate.clone();
        let admission = async move {
            for done in [1, 5, 10] {
                tell(&dl(done));
                tokio::task::yield_now().await;
            }
            let held = ours.lock().await;
            // As `Measuring` comes after `take_gate`.
            tell(&WaitPhase::Measuring);
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            drop(held);
            7
        };
        let theirs = gate.clone();
        let other = tokio::spawn(async move {
            tokio::task::yield_now().await;
            let _g = theirs.lock().await;
        });
        let (out, said) =
            tokio::time::timeout(Duration::from_secs(5), downloads_said(&tx, "c", admission))
                .await
                .expect("the admission ran to its end");
        assert_eq!(out, 7);
        assert!(said.downloaded);
        assert!(said.loading.is_some(), "loading began after the download");
        tokio::time::timeout(Duration::from_secs(5), other)
            .await
            .expect("another admission got the gate")
            .unwrap();
        // The client still has only the frame that filled its channel: the
        // ones that waited for room were dropped once admission settled.
        assert_eq!(state_of(&rx.try_recv().unwrap())["state"], "loading");
        assert!(rx.try_recv().is_err());
    }

    /// Once the client reads again, the newest frame goes out, not the
    /// oldest one: a newer download step replaced the one still waiting.
    #[tokio::test]
    async fn the_newest_frame_goes_out_once_there_is_room() {
        let (tx, mut rx) = stalled();
        let (go, wait) = tokio::sync::oneshot::channel::<()>();
        let admission = async move {
            tell(&dl(1));
            tokio::task::yield_now().await;
            tell(&dl(5));
            let _ = wait.await;
        };
        let client = async move {
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            let first = rx.recv().await.unwrap();
            assert_eq!(state_of(&first)["state"], "loading");
            let next = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("a frame once there was room")
                .unwrap();
            let next = state_of(&next);
            assert_eq!(next["state"], "downloading");
            assert_eq!(next["done_bytes"], 5, "{next}");
            go.send(()).unwrap();
        };
        let ((_, said), ()) = tokio::join!(downloads_said(&tx, "c", admission), client);
        assert!(said.downloaded);
        assert_eq!(said.loading, None, "the download was the last thing heard");
    }
}
