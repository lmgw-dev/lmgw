//! Warm a caller's models before their first request (realtime design
//! §9.1, chat-voice design §4.2–§4.3), in one of two modes ([`WarmMode`]).
//!
//! **Background** — warm early, never evict. At a realtime session's start
//! (`realtime.warm_on_connect`) and again at every `speech_started`, the
//! session's local models are started in the background, so a cold
//! container comes up while the user is still talking. It is the admission
//! warm starts at boot use (`VramScheduler::check_background_start`): a
//! start that fits goes ahead, one that would need an eviction is
//! **skipped** (`skipped: full`), and so is everything while the GPU hold
//! (or a benchmark's lease) is on (`held`, or `fallback` when the alias has
//! one that would answer). No claim is kept: the container is resident, not
//! held, so a quiet session pins nothing and a false `speech_started` from
//! noise costs nothing (§9.1). The holds that keep a model up are taken
//! where it is used: around the ASR call, and by the response. What it finds
//! with nothing to do — up already, an alias that does not resolve, a
//! disabled row, a voice that cannot speak — is a debug line: a session's
//! `speech_started` warms find the same every time.
//!
//! **Admit** — an explicit press (chat-voice §4.2: the Chat's mic, entering
//! realtime mode) warms through the request admission
//! (`gate::resolve(…).admit()`, [`admit`]), so it may evict idle models
//! exactly as the request it announces would, and it holds the claim while a
//! lazy audio row loads ([`load`], on that claim), then drops it. Under the
//! GPU hold it is answered as any request is — the alias's fallback, which a
//! warm never starts, or `gpu_hold`; the benchmark lease and `gpu_block` on
//! the placement of the descriptor started are the admission's own rules,
//! and a CPU audio row claims no VRAM and serves under the hold. Several
//! stages warm **as one group**: the group's footprint is checked first
//! against what lmgw may use, and if it fits the stages are admitted side by
//! side and every claim is kept until all of them are up, so a sibling
//! cannot evict a stage that has just loaded; if it does not fit, the group
//! reports `skipped: does_not_fit` with the sizes and falls back to a
//! Background warm — no eviction ping-pong. A stage that cannot get room
//! beside its siblings' kept claims gives its wait up (`does_not_fit`), so a
//! group never holds the admission gate on itself ([`kept`]). A caller that
//! goes away before admission (the press ended) drops the wait; a container
//! start already in flight finishes.
//!
//! Each stage ends in a [`WarmOutcome`], said to the caller as a `state`
//! frame ([`ModelState`], through a [`Reporter`]) and logged once.
//!
//! The word check's model is warmed too when `barge_in_check` is `words`
//! and it is not the session's ASR model (live run 3, D3: cold, the first
//! check timed out while its container loaded, and the duration rule
//! decided the barge). A cloud alias or a candidate alias (its pick is made
//! per request) needs nothing in Background; a model already up is not
//! started again, and is loaded when it has not loaded yet ([`load`]). One
//! Background warm at a time per session: a `speech_started` while the last
//! one is still running adds nothing.
//!
//! **A voice that can never speak is not warmed** ([`Warm::Voice`]): the
//! TTS row is judged as a response's open judges it
//! (`proxy::synthesize::refuse_route`) — a task its package does not run, or
//! a voice-design row with no description in the caller's speech
//! instructions or its own defaults — and skipped (`cannot_speak`), so no
//! container starts (and nothing is evicted later) for it. The response
//! that tries to speak reports the refusal.
//!
//! **A lazy audio model is loaded, not only started** ([`load`], live run
//! 3b, D3'). audio.cpp loads a lazy row's weights on its first request, and
//! its port answers before that: warmed by its container alone, the word
//! check's model held no GPU memory, the first barge's check loaded it and
//! timed out, and the first turn's transcript and first spoken clause paid
//! the load too. So once the container is up — started by this warm or
//! already there — a row that has not loaded its model is sent the smallest
//! real request: a short silent clip to a speech-to-text row, a one-word
//! clause in the caller's voice to the TTS. It is accounted like any request
//! (the model is busy while it loads, the pending load stops being charged,
//! the residency is learned), and it writes no request row — nobody asked
//! for it.
//!
//! **The models start side by side**, not one after the other: in the first
//! live acceptance the chat model's cold start (3.4 s) held the ASR model's
//! back, and the first turn waited for both in a row (§23). Concurrent
//! starts are safe here: each one's measure-and-claim takes the admission
//! gate, so two cannot be promised the same free bytes.

use std::time::Instant;

use crate::proxy::synthesize::SessionSeed;
use crate::proxy::{stop_pair, StopHandle, StopSignal};
use crate::state::SharedState;

mod admit;
mod background;
mod kept;
mod load;
mod outcome;

pub(crate) use outcome::{GroupSizes, HeldCause, ModelState, Reporter, SkipReason, WarmOutcome};

/// How a warm may make room (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WarmMode {
    /// Through the request admission: it may evict idle models, and it
    /// holds its claims until the group is up.
    Admit,
    /// Only into room there is: it never evicts, and keeps no claim.
    Background,
}

/// One of the caller's models to warm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Warm {
    /// A model by its alias, and the stage it serves in the `state` frames:
    /// `chat`, `asr`, or `check` (the barge-in word check's own).
    Model { stage: &'static str, alias: String },
    /// The TTS alias (stage `tts`), with the speech instructions its clauses
    /// carry: a row that cannot speak with them is not started (module
    /// doc).
    Voice {
        alias: String,
        instructions: Option<String>,
        /// How its clauses speak, for the load ([`load`]); `None` when the
        /// caller's voice resolves to nothing — a response would be
        /// refused, so nothing is loaded for it.
        speaks: Option<Speaks>,
    },
}

/// What the caller's clauses are sent besides their text: the voice
/// (`None` for one the engine speaks without being named), the language —
/// a session's hint or the Chat's configured one — and the seed, when there
/// is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Speaks {
    pub voice: Option<String>,
    pub language: Option<crate::audio::language::SpeechLanguage>,
    pub seed: Option<SessionSeed>,
}

impl Warm {
    pub(crate) fn alias(&self) -> &str {
        match self {
            Self::Model { alias, .. } | Self::Voice { alias, .. } => alias,
        }
    }

    /// The stage it is said as in a `state` frame.
    pub(crate) fn stage(&self) -> &'static str {
        match self {
            Self::Model { stage, .. } => stage,
            Self::Voice { .. } => "tts",
        }
    }
}

/// A realtime session's warming, at most one at a time.
pub(crate) struct Warmer {
    task: Option<tokio::task::JoinHandle<()>>,
    /// Raised when the session drops its warmer — the session ended: an
    /// Admit warm's [`Reporter`] goes with it, so a wait for admission is
    /// dropped as a press that ended would drop it.
    _alive: StopHandle,
    life: StopSignal,
}

impl Default for Warmer {
    fn default() -> Self {
        let (alive, life) = stop_pair();
        Self {
            task: None,
            _alive: alive,
            life,
        }
    }
}

impl Warmer {
    /// Warm `models` in the background of the session, unless a warm is
    /// still running. The outcomes go to the log.
    pub fn warm(
        &mut self,
        state: &SharedState,
        session_id: &str,
        mode: WarmMode,
        models: Vec<Warm>,
    ) {
        self.warm_to(state, session_id, mode, models, None);
    }

    /// [`Self::warm`], its outcomes also said to `states` (a bound
    /// session's `lmgw.model.state`, chat-voice design §8.7).
    pub fn warm_to(
        &mut self,
        state: &SharedState,
        session_id: &str,
        mode: WarmMode,
        models: Vec<Warm>,
        states: Option<tokio::sync::mpsc::UnboundedSender<ModelState>>,
    ) {
        if models.is_empty() || self.task.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let (state, label) = (state.clone(), format!("realtime {session_id}"));
        let report = match states {
            Some(tx) => Reporter::to_until(tx, self.life.clone()),
            None => Reporter::until(self.life.clone()),
        };
        // Not aborted with the session: a start in flight is the registry's,
        // and finishing it leaves a resident, unclaimed model. A wait for
        // admission goes with the session instead (`_alive`).
        self.task = Some(tokio::spawn(async move {
            warm_group(&state, &label, mode, &models, &report).await;
        }));
    }
}

impl Warmer {
    /// [`Self::warm`] of models known only once `models` resolves — a bound
    /// session's thread, read afresh at each `speech_started` (chat-voice
    /// §4.1, WP8 review m5), so a chip changed mid-session warms the model
    /// the next turn uses. The outcomes go to the log.
    pub fn warm_later(
        &mut self,
        state: &SharedState,
        session_id: &str,
        mode: WarmMode,
        models: impl std::future::Future<Output = Vec<Warm>> + Send + 'static,
    ) {
        if self.task.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let (state, label) = (state.clone(), format!("realtime {session_id}"));
        let report = Reporter::until(self.life.clone());
        self.task = Some(tokio::spawn(async move {
            let models = models.await;
            if !models.is_empty() {
                warm_group(&state, &label, mode, &models, &report).await;
            }
        }));
    }
}

/// Whether `class`'s model `model_id` is up and ready — and, for an audio
/// row, holds its model: a request to it now starts, waits for and loads
/// nothing (chat-voice design §4.3, the turn paths' `loading` frames). A
/// container starting for another caller, or on its way out, is a wait.
pub(crate) fn resident(
    state: &SharedState,
    snap: &crate::config::Snapshot,
    class: crate::runtime::Class,
    model_id: &str,
) -> bool {
    match class {
        crate::runtime::Class::Audio => load::up_and_loaded(state, snap, model_id),
        _ => state.runtime().ready_port(class, model_id).is_some(),
    }
}

/// Warm `models` side by side in `mode` (module doc): each stage's outcome
/// is said to `report` as it settles and logged under `label`; the
/// outcomes come back in `models`' order.
pub(crate) async fn warm_group(
    state: &SharedState,
    label: &str,
    mode: WarmMode,
    models: &[Warm],
    report: &Reporter,
) -> Vec<WarmOutcome> {
    match mode {
        WarmMode::Background => {
            let each = models
                .iter()
                .map(|m| background::one(state, label, m, report));
            futures::future::join_all(each).await
        }
        WarmMode::Admit => admit::group(state, label, models, report).await,
    }
}

/// Say and log `outcome` for `warm`. A model that was up already and had
/// nothing to load is a debug line, and so is anything `quiet` (a
/// Background warm that found nothing to do): a session's `speech_started`
/// warms find that every time.
fn settle(label: &str, warm: &Warm, report: &Reporter, outcome: &WarmOutcome, quiet: bool) {
    let (stage, alias) = (warm.stage(), warm.alias());
    report.outcome(stage, alias, outcome);
    let what = outcome.describe();
    if quiet || *outcome == (WarmOutcome::Ready { ms: None }) {
        tracing::debug!("{label}: warm of '{alias}' ({stage}): {what}");
    } else {
        tracing::info!("{label}: warm of '{alias}' ({stage}): {what}");
    }
}

/// When a stage's `loading` frame went out; it goes out once.
#[derive(Default)]
struct Loading(Option<Instant>);

impl Loading {
    fn mark(&mut self, report: &Reporter, warm: &Warm) {
        if self.0.is_none() {
            report.send(ModelState::loading(warm.stage(), warm.alias()));
            self.0 = Some(Instant::now());
        }
    }

    /// The load time, when anything loaded.
    fn ms(&self) -> Option<u64> {
        self.0.map(|t| t.elapsed().as_millis() as u64)
    }
}
