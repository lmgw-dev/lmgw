//! Loading a lazy audio model the warm has up (live run 3b, D3'; module doc
//! of [`super`]).
//!
//! Only an lmgw audio row loads lazily — a chat model's llama-server loads
//! its weights before it answers its port — and only one whose container
//! has not loaded its model yet (`VramScheduler::audio_model_loaded`: it
//! answered nothing, or audio.cpp unloaded it for idling since). What it
//! is sent is the smallest request the caller's own would be:
//! - a speech-to-text row (the ASR alias, the word check's): half a second
//!   of 16 kHz silence (`proxy::transcribe::warm`);
//! - the TTS: one word in the caller's voice, seed, language and speech
//!   instructions (`proxy::synthesize::warm`) — nothing when the caller
//!   has no voice it could speak with.
//!
//! The request goes on a claim of the running container, one of two:
//! - **Admit** (chat-voice design §4.2): the admission's own claim, which
//!   the warm took to start the model ([`send`]). It is kept while the row
//!   loads, and dropped once the warm's group is up.
//! - **Background** ([`by_join`]): an owner's join of the running container
//!   ([`crate::vram::join`], `Restart::No`): it never starts, queues for or
//!   evicts anything, it is refused under the GPU hold or a benchmark, and
//!   a container that went away meanwhile is not brought back for it.
//!
//! Either way each load is one log line, never a request row.

use std::time::Instant;

use crate::bench::lease::GpuBlock;
use crate::config::{AudioModel, Route, Snapshot};
use crate::error::GatewayError;
use crate::proxy::synthesize::{warm as synthesize_warm, ClauseSpeech};
use crate::proxy::transcribe_warm;
use crate::runtime::registry::{Origin, RuntimeState};
use crate::runtime::Class;
use crate::state::SharedState;
use crate::vram::{LocalHold, Restart};

use super::{Speaks, Warm};

/// What the TTS is sent to load: one word.
pub(crate) const WARM_INPUT: &str = "Hello.";

/// The speech-to-text load's clip: half a second at 16 kHz.
const SILENCE_SAMPLES: usize = 8_000;

/// What a warm sends its row.
pub(super) enum Job<'a> {
    Asr,
    Voice {
        speaks: &'a Speaks,
        instructions: Option<&'a str>,
    },
}

impl Job<'_> {
    fn what(&self) -> &'static str {
        match self {
            Self::Asr => "half a second of silence",
            Self::Voice { .. } => "a one-word clause",
        }
    }
}

/// The audio row `model_id` and what `warm` sends it to load — `None` when
/// it sends nothing: no such row, a speech-to-text warm of a row of another
/// task, or a voice the caller could not speak with.
pub(super) fn job<'a>(
    snap: &'a Snapshot,
    warm: &'a Warm,
    model_id: &str,
) -> Option<(&'a AudioModel, Job<'a>)> {
    let row = snap.audio_models.iter().find(|m| m.model_id == model_id)?;
    let job = match warm {
        Warm::Model { .. } if row.task == "asr" => Job::Asr,
        Warm::Voice {
            speaks: Some(speaks),
            instructions,
            ..
        } => Job::Voice {
            speaks,
            instructions: instructions.as_deref(),
        },
        _ => return None,
    };
    Some((row, job))
}

/// Whether `row`'s container of `generation` holds its model.
pub(super) fn loaded(
    state: &SharedState,
    snap: &Snapshot,
    row: &AudioModel,
    generation: u64,
) -> bool {
    state
        .vram
        .audio_model_loaded(row, &snap.settings.audio, generation)
}

/// Whether the audio row `model_id` is up and holds its model: a warm then
/// has nothing to do for it.
pub(super) fn up_and_loaded(state: &SharedState, snap: &Snapshot, model_id: &str) -> bool {
    let Some(row) = snap.audio_models.iter().find(|m| m.model_id == model_id) else {
        return false;
    };
    state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == Class::Audio && v.model_id == model_id)
        .is_some_and(|v| v.state == RuntimeState::Ready && loaded(state, snap, row, v.generation))
}

/// Send `job` to `alias`'s container on `hold`'s claim of it (module doc):
/// the milliseconds it took to answer, or why it failed. `label` names the
/// caller in the log line.
pub(super) async fn send(
    state: &SharedState,
    label: &str,
    alias: &str,
    job: &Job<'_>,
    route: &Route,
    hold: &LocalHold,
) -> Result<u64, GatewayError> {
    let mut route = route.clone();
    hold.point(&mut route);
    let model_id = hold.model_id().to_string();
    let started = Instant::now();
    let loaded = match job {
        Job::Asr => match crate::realtime::transcribe::wav_16k(&[0; SILENCE_SAMPLES]) {
            Ok(wav) => transcribe_warm::load(state, &route, hold, wav.into()).await,
            Err(e) => Err(e),
        },
        Job::Voice {
            speaks,
            instructions,
        } => {
            let clause = ClauseSpeech {
                input: WARM_INPUT,
                voice: speaks.voice.as_deref(),
                speed: None,
                language: speaks.language.as_ref(),
                instructions: *instructions,
                cue: None,
                seed: speaks.seed,
            };
            synthesize_warm::load(state, label, alias, &route, hold, &clause)
                .await
                .map(drop)
        }
    };
    let (ms, what) = (started.elapsed().as_millis() as u64, job.what());
    match &loaded {
        Ok(()) => tracing::info!(
            "{label}: loaded '{alias}' (audio '{model_id}') ahead of its first request: {what} \
             answered in {ms} ms (a warm-up, no request row)"
        ),
        Err(e) => tracing::info!(
            "{label}: loading '{alias}' (audio '{model_id}') ahead of its first request failed \
             after {ms} ms ({what}): {e} — its first request loads it"
        ),
    }
    loaded.map(|()| ms)
}

/// What a Background load came to.
pub(super) enum Joined {
    /// Nothing to send: no job, or the model is loaded already.
    NotNeeded,
    /// It was sent and answered.
    Loaded,
    /// The GPU hold or a benchmark: nothing local takes new work.
    Held(GpuBlock),
    /// No claim: the join was refused, or the container went away
    /// meanwhile.
    NoClaim(Option<GatewayError>),
    /// It was sent and failed.
    Failed(GatewayError),
}

/// Load `warm`'s audio row `model_id`, up on `route`, on a join of its
/// container unless it holds its model already (module doc). `loading` is
/// called once a load is about to be sent.
pub(super) async fn by_join(
    state: &SharedState,
    label: &str,
    warm: &Warm,
    route: &Route,
    model_id: &str,
    loading: impl FnOnce(),
) -> Joined {
    let snap = state.snapshot();
    let Some((row, job)) = job(&snap, warm, model_id) else {
        return Joined::NotNeeded;
    };
    // Loaded already: no claim — a `speech_started`'s warm leaves the model
    // as it found it.
    if up_and_loaded(state, &snap, model_id) {
        return Joined::NotNeeded;
    }
    // The GPU hold or a benchmark: nothing local takes new work, and the
    // start path has said so for a model it did not start. A row on the CPU
    // is not held: it is loaded at connect under the hold too — unless its
    // container still runs on the GPU from before the switch, which the join
    // below refuses.
    if let Some(block) = snap.gpu_block_for(Class::Audio, model_id) {
        return Joined::Held(block);
    }
    let alias = warm.alias();
    let hold = match crate::vram::join(state, route, alias, Origin::Owner, Restart::No).await {
        Ok(Some(hold)) => hold,
        // Gone meanwhile: nothing to load, and nothing is started for it.
        Ok(None) => return Joined::NoClaim(None),
        Err(e) => {
            tracing::info!(
                "{label}: not loading '{alias}' (audio '{model_id}') ahead of its first request: \
                 {e}"
            );
            return Joined::NoClaim(Some(e));
        }
    };
    // Another request may have loaded it meanwhile.
    if loaded(state, &snap, row, hold.generation()) {
        return Joined::NotNeeded;
    }
    loading();
    match send(state, label, alias, &job, route, &hold).await {
        Ok(_) => Joined::Loaded,
        Err(e) => Joined::Failed(e),
    }
}
