//! Does a thread's voice turn go to its chat model as audio
//! (voice-audio-input design §2)? The setting as the thread resolves it
//! ([`setting`]: the thread's own, then Settings → Chat → Voice), and the
//! **verdict** ([`verdict`]): §2.2's table, row by row, the first row that
//! fails giving the transcript path and its `why`.
//!
//! | row | fails with |
//! |---|---|
//! | the setting is `on` | "audio input is off (…)" |
//! | no knowledge bases in auto mode | they search with the turn's words |
//! | a speech-to-text alias that resolves | no speech recognition is set up |
//! | `gate::resolve` settles a route (hold and benchmark swaps, a candidate alias's fallback) | the resolve's own error |
//! | no context guard (a ladder row, a guarded pool; every routable candidate of a candidate alias) | it guards its context |
//! | the model that answers hears ([`crate::capabilities::hears`]: an egress with an audio part, a chat model whose `input_modalities` include `audio`, a server that did not say it loaded no audio projector) | it does not, or lmgw cannot tell |
//!
//! **Capability, not locality** (changed 2026-10-06, the owner's ruling):
//! where the model runs plays no role. A configured fallback is always
//! used, and hears the turn when it takes audio, wherever it runs.
//!
//! **What it foresees.** The swaps `gate::resolve` makes: under the GPU hold
//! (or a benchmark's lease) a model, a candidate alias too, is judged by the
//! fallback it hands the turn to, and by nothing else — the page never says
//! the model hears the user while a fallback that does not would answer.
//! With no block live, the verdict that block would bring is computed beside
//! ([`AudioInput::blocked`]), for the page that sees a block before it asks
//! again. A candidate alias's walk needs no forecast: with the Audio facet
//! on, every candidate and its fallback take audio as published
//! (candidate-aliases §4.6), and with it off the alias does not. The verdict
//! and the send agree in the common case; a mismatch (a pick whose server
//! says otherwise) is refused at the send and retried as the transcript. **What it cannot foresee:** a swap
//! the gate makes later, at admission (§4.7's outside-VRAM verdict), in a
//! climb or in the walk over loaded candidates, and a hold switched on after
//! the verdict. It is a prediction, never a permission: `fit_route` asks the
//! same predicate of the model that answers before a byte leaves
//! (`spoken::may_hear`), and a turn refused there goes again as its
//! transcript (§3.4, §3.5). Unknown is no.
//!
//! Nothing is started: `gate::resolve` claims nothing, the capability
//! lookup is the one attachments make (`capabilities::exposed`), for the
//! name the resolve settled on, and a managed server's facts are read from
//! the container that is up, if one is. The session-only row — the model's
//! server refused audio this session (§3.5) — is the bound session's, on top
//! of this ([`AudioInput::after_refusal`]).

use std::collections::HashMap;

use serde::Serialize;

use crate::capabilities::hears::{self, Hears};
use crate::config::{FallbackRoute, Route, Snapshot};
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
    /// What handed the turn to `model` when the resolve swapped ("under the
    /// GPU hold", "while a benchmark run holds the GPU"): the page says the
    /// verdict after it, and knows the verdict is the block's already.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lead: Option<&'static str>,
    /// The verdict a GPU block would bring (decision D3, 2026-10-06): its
    /// fallback's, or the refusal when there is none. Computed only when no
    /// block is live now and one would hand the turn elsewhere (a chat model
    /// lmgw runs, a candidate alias), after the thread's own rows passed;
    /// the page shows it while it sees a block before it asks again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<Box<AudioInput>>,
    /// The models an audio turn may reach, as a turn names who answered
    /// (`chat_turn::answered_by`, else the thread's model): the model
    /// itself, a swap's fallback, or a candidate alias's routable candidates
    /// by their public names. What the session's refusal memory is matched
    /// against ([`Self::after_refusal`]); never shown.
    #[serde(skip)]
    pub via: Vec<String>,
    /// The transcript is because the model it goes to lacks audio input,
    /// or refused it this session: what the turn's request rows then say
    /// (`request_logs.degraded`). `false` for any other reason (a guard,
    /// a thread row, a model list still being read).
    #[serde(skip)]
    pub lacks: bool,
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
            lead: None,
            blocked: None,
            via: Vec::new(),
            lacks: false,
        }
    }

    /// The verdict's session row (§2.2, §3.5): once the server of every
    /// model an audio turn may reach refused the audio this session, or
    /// failed on it (`refused`, the bound session's memory by model), its
    /// later turns go as their transcript. A model that did not — the
    /// thread's model changed since, or a candidate alias may still pick
    /// one that did not — still hears. The block's verdict
    /// ([`Self::blocked`]) is held to the same memory.
    pub(crate) fn after_refusal(mut self, refused: &Refusals) -> Self {
        self.blocked = self
            .blocked
            .take()
            .map(|b| Box::new(b.after_refusal(refused)));
        if self.path != InputPath::Audio || self.via.is_empty() {
            return self;
        }
        let whys: Option<Vec<&String>> = self.via.iter().map(|m| refused.get(m)).collect();
        match whys.as_deref() {
            Some([why, ..]) => Self {
                path: InputPath::Transcript,
                why: Some((*why).clone()),
                via: Vec::new(),
                lacks: true,
                ..self
            },
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
/// it; a stored `local`, the value's name before 2026-10-06, is `on`).
pub(crate) fn setting(snap: &Snapshot, thread: &ChatThread) -> (AudioInputMode, Source) {
    match thread.voice.audio_input {
        Some(m) => (m, Source::Thread),
        None => (
            AudioInputMode::parse_stored(&snap.settings.chat_voice_audio_input).unwrap_or_default(),
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
    let route = routed.resolved();
    // The resolve swapped (the hold, a benchmark's lease, a CPU row's GPU
    // container): only the fallback answers, so only it is judged — no
    // candidate walk, no candidate's guard.
    if let Some(fallback) = headers.fallback() {
        let lead = lead(headers.fallback_reason());
        return judge(state, &snap, fallback, route, None, Said::After(lead)).await;
    }
    // A candidate alias the resolve did not swap: the candidates its walk
    // may pick, and its fallback (the gate's pick, as its walk reads it).
    let walk = match snap.candidate_alias(alias) {
        Some(ca) => Some(candidate::Walk::read(state, &snap, ca).await),
        None => None,
    };
    let mut v = judge(state, &snap, alias, route, walk.as_ref(), Said::Own).await;
    v.blocked = blocked(state, &snap, alias, route, walk.as_ref())
        .await
        .map(Box::new);
    v
}

/// How a verdict's `why` names the model it judged.
#[derive(Debug, Clone, Copy)]
enum Said {
    /// The thread's own model: "{name} does not take audio input".
    Own,
    /// A swap the resolve made: "{lead} this goes to {name}, which …".
    After(&'static str),
    /// A block's forecast ([`AudioInput::blocked`]), which the page puts its
    /// own words for the block in front of: "this goes to {name}, which …".
    Forecast,
}

/// The verdict for `name` answering on `route`: a context guard (a
/// candidate alias's `walk` judges its candidates'), then the capability
/// predicate.
async fn judge(
    state: &SharedState,
    snap: &Snapshot,
    name: &str,
    route: &Route,
    walk: Option<&candidate::Walk>,
    said: Said,
) -> AudioInput {
    let lead = match said {
        Said::After(lead) => Some(lead),
        Said::Own | Said::Forecast => None,
    };
    let guarded = match walk {
        Some(w) => w.guard(snap),
        None => crate::vram::classify(route)
            .filter(|t| t.class == Class::Chat)
            .and_then(|t| guard(snap, &t.model_id)),
    };
    if let Some(why) = guarded {
        return AudioInput {
            lead,
            ..AudioInput::transcript(name, why)
        };
    }
    // The capabilities of the name the resolve settled on — not resolved
    // again, which a hold switched on meanwhile would hand to its fallback.
    // The thread view never waits on a provider (review V9): a catalog is
    // read from the cache alone, a cold one in the background, and until it
    // is in the model is not known yet — the transcript, saying so. The send
    // reads it in full and stays the authority.
    let facts = hears::server_facts(state, None, route);
    let asked = hears::hears(state, hears::Model::Named(name), route, facts.as_deref());
    let (heard, unread) = crate::catalog::cached_only(asked).await;
    let why = match (heard, unread) {
        (Hears::Unknown, true) => Some(match said {
            Said::Own => format!(
                "lmgw is still reading the model list {name} is in, so it cannot tell yet \
                 whether it takes audio"
            ),
            Said::After(lead) => {
                format!("{lead} this goes to {name}, whose model list lmgw is still reading")
            }
            Said::Forecast => {
                format!("this goes to {name}, whose model list lmgw is still reading")
            }
        }),
        _ => match said {
            Said::Own => heard.why(name),
            Said::After(lead) => heard.goes_to(name).map(|w| format!("{lead} {w}")),
            Said::Forecast => heard.goes_to(name),
        }
        .map(|w| with_hint(snap, name, heard, w)),
    };
    match why {
        None => AudioInput {
            path: InputPath::Audio,
            model: name.to_string(),
            why: None,
            lead,
            blocked: None,
            via: match walk {
                Some(w) => w.via(snap),
                None => vec![name.to_string()],
            },
            lacks: false,
        },
        Some(why) => AudioInput {
            lead,
            lacks: matches!(heard, Hears::No(_)),
            ..AudioInput::transcript(name, why)
        },
    }
}

/// How the owner makes a model hear that lmgw cannot judge (decision D1),
/// said after the `why` of an unknown, in the words for where `name` lives
/// (review V5): an alias's or a local row's capabilities override — with
/// the task, which an override needs when the catalog says nothing — and
/// for a passthrough model, which has no override of its own, an alias.
fn with_hint(snap: &Snapshot, name: &str, heard: Hears, why: String) -> String {
    if heard != Hears::Unknown {
        return why;
    }
    let row = snap
        .local_models
        .iter()
        .any(|m| snap.local_public_name(&m.model_id) == name);
    let hint = if snap.aliases.contains_key(name) {
        "if it does, give its alias a capabilities override with task chat and input \
         modalities text and audio"
    } else if row {
        "if it does, list audio in the input modalities of its row's capabilities override"
    } else {
        "if it does, make it an alias whose capabilities override says task chat and input \
         modalities text and audio (a passthrough model has no override of its own)"
    };
    format!("{why}: {hint}")
}

/// The words a swap of `reason` is said with.
fn lead(reason: Option<FallbackReason>) -> &'static str {
    match reason {
        Some(FallbackReason::Benchmark) => "while a benchmark run holds the GPU",
        Some(FallbackReason::Hold) | None => "under the GPU hold",
        Some(_) => "as a fallback",
    }
}

/// The verdict a GPU block would bring for `alias`, resolved to `route`
/// with no block live ([`AudioInput::blocked`]): `None` for a route no
/// block touches (a model lmgw does not run). The block hands the turn to
/// the fallback the gate's swap takes (`Snapshot::request_fallback`, a
/// candidate alias's own included), judged by itself; with none usable the
/// turn is refused. The page puts its own words for the block in front.
async fn blocked(
    state: &SharedState,
    snap: &Snapshot,
    alias: &str,
    route: &Route,
    walk: Option<&candidate::Walk>,
) -> Option<AudioInput> {
    let target = crate::vram::classify(route).filter(|t| t.class == Class::Chat)?;
    let refused = |why: String| Some(AudioInput::transcript(alias, why));
    let (fallback, route) = match snap.request_fallback(alias, &target) {
        FallbackRoute::None => {
            return refused(format!("{alias} has no fallback, so the turn is refused"))
        }
        FallbackRoute::Unusable { alias: fb, why } => {
            return refused(format!(
                "{alias}'s fallback {fb} {why}, so the turn is refused"
            ))
        }
        // A candidate alias's fallback that the walk could not use (it does
        // not resolve, or is local). One that lacks a facet the alias
        // enables is used (changed 2026-10-06): a turn it cannot hear goes
        // to it as its transcript, which `judge` says.
        FallbackRoute::Usable { alias: fb, .. } if walk.is_some_and(|w| !w.fallback_usable()) => {
            return refused(format!(
                "{alias}'s fallback {fb} cannot be used, so the turn is refused"
            ))
        }
        FallbackRoute::Usable { alias: fb, route } => (fb, route),
    };
    Some(judge(state, snap, &fallback, &route, None, Said::Forecast).await)
}

/// The rows a thread decides without a route: the setting, and auto-mode
/// knowledge bases (they search with the turn's words, which an audio turn
/// does not have at launch). `None` when both pass. The bound session's
/// responder re-reads these two before each turn (§3.5).
pub(crate) fn thread_rows(snap: &Snapshot, thread: &ChatThread) -> Option<String> {
    match setting(snap, thread) {
        (AudioInputMode::On, _) => {}
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
