//! Does a thread's voice turn go to its chat model as audio
//! (voice-audio-input design §2)? The setting as the thread resolves it
//! ([`setting`]: the thread's own, then Settings → Chat → Voice), and the
//! **verdict** ([`verdict`]): §2.2's table, row by row, the first row that
//! fails giving the transcript path and its `why`.
//!
//! | row | fails with |
//! |---|---|
//! | the setting is `local` | "audio input is off (…)" |
//! | no knowledge bases in auto mode | they search with the turn's words |
//! | a speech-to-text alias that resolves | no speech recognition is set up |
//! | `gate::resolve` settles a route (hold and benchmark swaps, a candidate alias's fallback) | the resolve's own error |
//! | that route is a chat model this lmgw runs (`vram::classify`) | a model lmgw does not run, or a server lmgw does not run |
//! | a candidate alias's walk does not end at its fallback by design (a background alias, a primary it cannot use) | it may go to a model lmgw does not run |
//! | no context guard (a ladder row, a guarded pool; every routable candidate of a candidate alias) | it guards its context |
//! | its `input_modalities` include `audio` | it does not, or lmgw cannot tell |
//!
//! **Local means run by this lmgw** (decision 5): `vram::classify(&route)
//! .is_some()`, not `Snapshot::is_local_upstream`, which is true for any
//! llama-server wherever it runs (right for pricing, wrong here; it only
//! picks the words for a server lmgw does not run).
//!
//! **What it foresees.** The swaps `gate::resolve` makes: under the GPU hold
//! (or a benchmark's lease) a model, a candidate alias too, is judged by the
//! fallback it hands the turn to, so the page never says the model hears the
//! user while a cloud model would answer. And a candidate alias whose walk
//! goes to its alias fallback by design — background traffic, which takes the
//! fallback whenever its primary is not loaded, or a primary it cannot use at
//! all — is judged by that fallback too, from the gate's own pick
//! (`candidates::derive::cached_pick`). **What it cannot foresee:** a swap
//! the gate makes later, at admission (§4.7's outside-VRAM verdict), in a
//! climb or in the walk over loaded candidates, and a hold switched on after
//! the verdict. It is a prediction, never a permission: `fit_route`'s
//! `local_only` refuses such a route before a byte leaves, and the turn goes
//! again as its transcript (§3.4, §3.5). Unknown is no.
//!
//! Nothing is started: `gate::resolve` claims nothing, and the capability
//! lookup is the one attachments make (`capabilities::exposed`), for the
//! name the resolve settled on. The session-only row — the model's server
//! refused audio this session (§3.5) — is the bound session's, on top of
//! this ([`AudioInput::after_refusal`]).

use std::collections::HashMap;

use serde::Serialize;

use crate::config::{Route, Snapshot};
use crate::gate::{self, FallbackReason, RouteCheck};
use crate::runtime::Class;
use crate::state::SharedState;
use crate::store::{AudioInputMode, ChatThread, InputPath, KbMode};

use super::resolve::Source;

mod candidate;
#[cfg(test)]
mod tests;

/// The verdict (module doc).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct AudioInput {
    /// `audio` when every row passes; `transcript` otherwise.
    pub path: InputPath,
    /// The model the turn goes to: the thread's, or the fallback a block
    /// hands it to.
    pub model: String,
    /// Why the transcript, in the page's words; `None` on the audio path.
    pub why: Option<String>,
    /// The models an audio turn may reach, as a turn names who answered
    /// (`chat_turn::answered_by`, else the thread's model): the model
    /// itself, or a candidate alias's routable candidates by their public
    /// names. What the session's refusal memory is matched against
    /// ([`Self::after_refusal`]); never shown.
    #[serde(skip)]
    pub via: Vec<String>,
}

/// The bound session's memory of the models whose server refused a heard
/// turn's audio, or failed on it (§3.5): the model, as a turn names who
/// answered, and the verdict's `why` for it from then on.
pub(crate) type Refusals = HashMap<String, String>;

impl AudioInput {
    fn transcript(model: &str, why: String) -> Self {
        Self {
            path: InputPath::Transcript,
            model: model.to_string(),
            why: Some(why),
            via: Vec::new(),
        }
    }

    /// The verdict's session row (§2.2, §3.5): once the server of every
    /// model an audio turn may reach refused the audio this session, or
    /// failed on it (`refused`, the bound session's memory by model), its
    /// later turns go as their transcript. A model that did not — the
    /// thread's model changed since, or a candidate alias may still pick
    /// one that did not — still hears.
    pub(crate) fn after_refusal(self, refused: &Refusals) -> Self {
        if self.path != InputPath::Audio || self.via.is_empty() {
            return self;
        }
        let whys: Option<Vec<&String>> = self.via.iter().map(|m| refused.get(m)).collect();
        match whys.as_deref() {
            Some([why, ..]) => Self::transcript(&self.model, (*why).clone()),
            _ => self,
        }
    }
}

/// `voice_resolved.audio_input`: the setting, the level it came from, and
/// the verdict (§2.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Shown {
    pub value: AudioInputMode,
    pub source: Source,
    #[serde(flatten)]
    pub verdict: AudioInput,
}

/// The thread's audio-input setting: its own, else Settings → Chat →
/// Voice's (a value that does not parse is `off`, as the load normalises
/// it).
pub(crate) fn setting(snap: &Snapshot, thread: &ChatThread) -> (AudioInputMode, Source) {
    match thread.voice.audio_input {
        Some(m) => (m, Source::Thread),
        None => (
            AudioInputMode::parse(&snap.settings.chat_voice_audio_input).unwrap_or_default(),
            Source::Chat,
        ),
    }
}

/// [`verdict`] with the setting it was judged under, for the thread JSON.
pub(crate) async fn shown(state: &SharedState, thread: &ChatThread) -> Shown {
    let (value, source) = setting(&state.snapshot(), thread);
    Shown {
        value,
        source,
        verdict: verdict(state, thread).await,
    }
}

/// Does `thread`'s next voice turn go as audio (module doc)?
pub(crate) async fn verdict(state: &SharedState, thread: &ChatThread) -> AudioInput {
    let snap = state.snapshot();
    let alias = thread.model_alias.as_str();
    if let Some(why) = thread_rows(&snap, thread) {
        return AudioInput::transcript(alias, why);
    }
    if let Some(why) = no_asr(&snap, thread) {
        return AudioInput::transcript(alias, why);
    }
    let routed = match gate::resolve(state, alias, RouteCheck::None).await {
        Ok(r) => r,
        Err(e) => return AudioInput::transcript(alias, e.error.to_string()),
    };
    let headers = routed.headers();
    let model = headers.fallback().unwrap_or(alias);
    let route = routed.resolved();
    if let Some(why) = not_run_here(&snap, route, model, headers.fallback_reason()) {
        return AudioInput::transcript(model, why);
    }
    // A candidate alias the resolve did not swap: the walk's own fallback,
    // and the candidates it may pick (the gate's pick, as its walk reads it).
    let walk = match snap.candidate_alias(alias) {
        Some(ca) => Some(candidate::Walk::read(state, &snap, ca).await),
        None => None,
    };
    if let Some(why) = walk.as_ref().and_then(|w| w.may_fall_back(&snap)) {
        return AudioInput::transcript(model, why);
    }
    let guarded = match &walk {
        Some(w) => w.guard(&snap),
        None => guard(&snap, &route.upstream_model),
    };
    if let Some(why) = guarded {
        return AudioInput::transcript(model, why);
    }
    // The capabilities of the name the resolve settled on — not resolved
    // again, which a hold switched on meanwhile would hand to its fallback.
    let audio = crate::capabilities::exposed::exposed_entry(state, model)
        .await
        .and_then(|e| e.capabilities)
        .and_then(|c| c.input_modalities)
        .map(|m| m.iter().any(|x| x == "audio"));
    match audio {
        Some(true) => AudioInput {
            path: InputPath::Audio,
            model: model.to_string(),
            why: None,
            via: match &walk {
                Some(w) => w.via(&snap),
                None => vec![model.to_string()],
            },
        },
        Some(false) => AudioInput::transcript(model, format!("{model} does not take audio input")),
        None => AudioInput::transcript(
            model,
            format!("lmgw cannot tell whether {model} takes audio"),
        ),
    }
}

/// The rows a thread decides without a route: the setting, and auto-mode
/// knowledge bases (they search with the turn's words, which an audio turn
/// does not have at launch). `None` when both pass. The bound session's
/// responder re-reads these two before each turn (§3.5).
pub(crate) fn thread_rows(snap: &Snapshot, thread: &ChatThread) -> Option<String> {
    match setting(snap, thread) {
        (AudioInputMode::Local, _) => {}
        (AudioInputMode::Off, Source::Thread) => {
            return Some("audio input is off (this thread)".into());
        }
        (AudioInputMode::Off, _) => {
            return Some("audio input is off (Settings → Chat → Voice)".into());
        }
    }
    (!thread.kb_ids.is_empty() && thread.kb_mode == KbMode::Auto)
        .then(|| "its knowledge bases search with your words (auto mode)".into())
}

/// Why a turn cannot go as audio for want of speech recognition, or `None`
/// (WP3 review #2): only the transcript tells a turn with words from noise
/// (the veto, §3.2), so a thread whose speech-to-text alias is unset at
/// every level, or names nothing lmgw knows, reads the transcript — and its
/// turns fail to transcribe as they do with audio input off. Not among the
/// rows the responder re-reads: a chip cleared after the commit fails that
/// turn's transcription, which then is not heard (§3.2).
fn no_asr(snap: &Snapshot, thread: &ChatThread) -> Option<String> {
    const TELLS: &str = "only a transcript tells your words from noise";
    match super::resolve::asr_alias(snap, thread) {
        None => Some(format!("no speech recognition is set up, and {TELLS}")),
        Some(asr) => snap
            .resolve(&asr)
            .err()
            .map(|e| format!("its speech recognition {asr} does not resolve ({e}), and {TELLS}")),
    }
}

/// Why `route` (answering as `model`) is no chat model this lmgw runs, or
/// `None` when it is one. A fallback is named with what handed the turn to
/// it.
fn not_run_here(
    snap: &Snapshot,
    route: &Route,
    model: &str,
    swapped: Option<FallbackReason>,
) -> Option<String> {
    let (what, tail) = match crate::vram::classify(route) {
        Some(t) if t.class == Class::Chat => return None,
        Some(_) => ("not a chat model", ""),
        None if snap.is_local_upstream(route.upstream.id) => (
            "served by a server this lmgw does not run",
            ": your voice goes only to models lmgw runs",
        ),
        // A cloud provider, or any other server on the network: lmgw cannot
        // tell them apart, and neither gets the audio.
        None => (NOT_RUN, STAYS),
    };
    let lead = match swapped {
        None => return Some(format!("{model} is {what}{tail}")),
        Some(FallbackReason::Hold) => "under the GPU hold this goes to",
        Some(FallbackReason::Benchmark) => "while a benchmark run holds the GPU this goes to",
        Some(_) => "this goes to its fallback",
    };
    Some(format!("{lead} {model}, {what}{tail}"))
}

/// What a model the audio never goes to is called (§2.2).
const NOT_RUN: &str = "a model lmgw does not run";
/// …and what that means for the voice.
const STAYS: &str = ": your voice stays on this machine";

/// Why local chat row `id` guards its context — a ladder row or a guarded
/// shared KV pool, which cannot bound an audio part
/// (`gate::count::media_bound`) — or `None`. Named as a turn names it.
fn guard(snap: &Snapshot, id: &str) -> Option<String> {
    snap.local_models
        .iter()
        .any(|m| m.model_id == id && (m.is_ladder() || m.params.pool_guarded()))
        .then(|| {
            format!(
                "{} guards its context, which cannot bound audio",
                snap.local_public_name(id)
            )
        })
}
