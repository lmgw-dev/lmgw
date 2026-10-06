//! Tool-result images on a llama.cpp route (llama egress design §8.2): the
//! conservative predicate that decides, once per send, whether they may go
//! as images at all ([`tool_image_predicate`], `decide`), and [`tool_media`], what
//! they add to the count when they may.
//!
//! Tool images arrive unasked: a tool returns a screenshot whether or not the
//! model behind the route can see it. So they go only where every condition
//! holds, and anywhere else as a placeholder naming the first condition that
//! failed, with a WARN; nothing is ever refused for them (decision 18). A
//! server that did not say whether it sees gets today's bytes, with no
//! reason of its own (decision 14).
//!
//! **Decided once per send** (§3.2): `decide` runs in `fit_chat`, before
//! its unguarded early return, and in the count that builds a body without a
//! send (`count_on_template`), on the same inputs. The decision travels on
//! the send's lease to `send_gated`, which sets it on the route every attempt
//! is built from (`on_route`); `LocalHold::point_at` carries it unchanged. A
//! candidate re-pick is a new route, fitted and decided for itself.
//!
//! **Rechecked per attempt, only ever downward** (`recheck`). A ladder
//! climb and a dead-container retry land on another container: a climb on
//! another rung, a retry on one re-admitted from the row as it is now, which
//! may have been edited. Before each attempt, and before each count that
//! attempt is judged by, a decision that let tool images go is held against
//! that container's own facts and advisory, and becomes its placeholder
//! where it no longer sees, carries an advisory or reads no projector, or
//! today's bytes where it says nothing. It never turns a placeholder into an
//! image. This is the one deliberate case of a send posting other than what
//! was counted, and it posts less: a placeholder of about 25 tokens where
//! the count reserved the image's bound. The guarded bound and the
//! candidate's facet are not read again: the alias is the same, and the
//! bound only ever let an image go that the count already holds.
//!
//! [`super::media_parts`] stays the count of *user* media: it runs before any
//! route exists, and a tool image never makes a request need Vision.
//!
//! **Outside the guarantee** (§8.2 "Limits"). The predicate guards the
//! llama.cpp egress, so only `llama_cpp` rows. Two paths reach a
//! llama-server without it: an `anthropic` + `llama_server` row, whose
//! `tool_result` images the Anthropic egress sends natively and llama-server
//! converts itself (no vision, ubatch or format check here); and native
//! `/v1/responses` passthrough on a row of another protocol in front of a
//! llama-server, which forwards the client's body verbatim (closed for
//! `llama_cpp` rows by decision 19). `decide` answers nothing for either.

use std::borrow::Cow;
use std::sync::Arc;

use super::count::{image_token_bound, ImageBound};
use super::facts::GateFacts;
use crate::candidates::{Facet, FacetSet};
use crate::config::{LlamaRoute, Protocol, Route, ToolImages};
use crate::egress::llama_cpp::tool_results::image_format;
use crate::ir::{ChatRequest, ContentPart, Role, ToolResultBlock};
use crate::runtime::registry::LlamaEntry;
use crate::state::SharedState;
use crate::vram::LocalHold;

/// The server said it has no vision (`/props` `modalities.vision: false`).
pub const NO_VISION: &str = "this model's server has no vision";
/// The managed row was started with a projector ubatch advisory: a
/// non-causal projector (Gemma 4) aborts llama-server on an image larger than
/// the ubatch, which kills every request in flight.
pub const PROJECTOR_CAN_ABORT: &str = "its projector can abort above the batch size";
/// The server sees, but the managed row loads its projector in a way lmgw
/// cannot read (`--mmproj-url`, the one `-hf` fetches by itself), so whether
/// its images are decoded non-causally is unknown, and its batch sizes are
/// below what an unknown projector needs (`unread_projector_short`).
pub const PROJECTOR_UNREAD: &str =
    "lmgw cannot read the projector it loads, and its batch size is below a whole image";
/// A guarded row (shared pool, ladder) whose per-image token bound is not
/// known: the gate could not count the image against the context.
pub const NO_IMAGE_BOUND: &str = "no per-image bound";
/// A candidate alias's request, and the alias does not enable Vision.
pub const CANDIDATE_WITHOUT_VISION: &str = "the candidate alias does not enable Vision";

/// What [`tool_image_predicate`] reads, each as the send found it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolImageInputs<'a> {
    /// `/props` `modalities.vision`; `None` when the server did not say.
    pub vision: Option<bool>,
    /// A managed row's projector ubatch advisory, as the container was
    /// started (`LlamaEntry::ubatch_advisory`). `None` when it has none, and
    /// on every external row.
    pub ubatch_advisory: Option<&'a str>,
    /// A managed row that loads no projector lmgw can read, started with
    /// batch sizes below what an unknown projector needs
    /// (`unread_projector_short`). `false` on every external row.
    pub unread_projector_short: bool,
    /// On a guarded row (a shared pool or a ladder), whether its per-image
    /// token bound is known (`gate::image_token_bound` gave one). `None` on
    /// an unguarded row, which counts nothing, and where no tool image would
    /// go (`decide` reads the bound only when it can matter).
    pub guarded_bound_known: Option<bool>,
    /// On a candidate alias's request, whether the alias enables the Vision
    /// facet — on every route the request takes, a fallback without a hold
    /// included. `None` for every other request.
    pub candidate_vision: Option<bool>,
}

/// §8.2's predicate, pure: [`ToolImages::Allowed`] when every condition
/// holds, else [`ToolImages::Refused`] with the reason of the first that
/// fails, in the spec's order (vision, the ubatch advisory or a projector
/// lmgw cannot read, the bound, the candidate's facet). A server that did
/// not say whether it sees is [`ToolImages::Unknown`]: today's bytes
/// (decision 14).
pub fn tool_image_predicate(inputs: &ToolImageInputs<'_>) -> ToolImages {
    let refused = |why: &str| ToolImages::Refused(why.to_string());
    match inputs.vision {
        None => ToolImages::Unknown,
        Some(false) => refused(NO_VISION),
        Some(true) if inputs.ubatch_advisory.is_some() => refused(PROJECTOR_CAN_ABORT),
        Some(true) if inputs.unread_projector_short => refused(PROJECTOR_UNREAD),
        Some(true) if inputs.guarded_bound_known == Some(false) => refused(NO_IMAGE_BOUND),
        Some(true) if inputs.candidate_vision == Some(false) => refused(CANDIDATE_WITHOUT_VISION),
        Some(true) => ToolImages::Allowed,
    }
}

/// What one chat send decided about its tool images ([`decide`]).
#[derive(Debug, Default)]
pub(crate) struct Decided {
    /// What the send's route carries (`Upstream.llama`): `None` off
    /// llama.cpp and while nothing is known about the server.
    pub(crate) llama: Option<Arc<LlamaRoute>>,
    /// The tool images the count adds at the per-image bound: [`tool_media`]
    /// when the decision allows them, else none.
    pub(crate) images: u64,
    /// The guarded row's per-image bound, when deciding read it, so the
    /// count does not read it again.
    pub(crate) bound: Option<Result<Option<ImageBound>, String>>,
}

/// Decide one chat send's tool images on `route` (module doc): the server's
/// facts ([`crate::llama_facts::resolve`], never waiting), the managed row's
/// ubatch advisory, the per-image bound of a guarded row (`guard`, the
/// facts its container was started with) and a candidate alias's Vision
/// facet, through [`tool_image_predicate`].
pub(crate) async fn decide(
    state: &SharedState,
    hold: Option<&LocalHold>,
    route: &Route,
    ir: &ChatRequest,
    guard: Option<&GateFacts>,
) -> Decided {
    if route.upstream.protocol != Protocol::LlamaCpp {
        return Decided::default();
    }
    let server = crate::llama_facts::resolve(state, hold, route);
    let Some(facts) = server.facts else {
        return Decided::default();
    };
    let sendable = tool_media(ir, facts.video == Some(true));
    let candidate_vision = candidate_facets(state, hold, ir).map(|e| e.contains(Facet::Vision));
    let started = hold.and_then(LocalHold::gate_facts);
    let mut inputs = ToolImageInputs {
        vision: facts.vision,
        ubatch_advisory: server.ubatch_advisory.as_deref(),
        unread_projector_short: started.as_deref().is_some_and(unread_projector_short),
        guarded_bound_known: None,
        candidate_vision,
    };
    let mut bound = None;
    if let Some(guard) = guard.filter(|_| sendable > 0) {
        if tool_image_predicate(&inputs).allowed() {
            let read = image_token_bound(&guard.models_dir, guard.projector_row()).await;
            inputs.guarded_bound_known = Some(matches!(read, Ok(Some(_))));
            bound = Some(read);
        }
    }
    let decision = tool_image_predicate(&inputs);
    Decided {
        images: if decision.allowed() { sendable } else { 0 },
        llama: Some(Arc::new(LlamaRoute {
            facts,
            tool_images: decision,
        })),
        bound,
    }
}

/// The facets a candidate alias's request is held to, on every route it
/// takes: the alias the request's hold was picked for
/// ([`super::candidate::fallback_facets`], which keeps the alias as picked if
/// it was disabled meanwhile), else the alias the request names. The second
/// is how a route without a hold is judged — the alias fallback under the
/// GPU hold, a climb's or an outside-VRAM verdict's fallback — so a
/// candidate alias that does not enable Vision sends no tool image through
/// any of its routes. `None` for every other request.
fn candidate_facets(
    state: &SharedState,
    hold: Option<&LocalHold>,
    ir: &ChatRequest,
) -> Option<FacetSet> {
    let snap = state.snapshot();
    hold.and_then(|h| super::candidate::fallback_facets(&snap, h))
        .or_else(|| {
            snap.candidate_alias(&ir.model_alias)
                .map(|a| FacetSet::from_names(&a.capabilities_enabled).unwrap_or_default())
        })
}

/// Whether a managed container's start row loads no projector lmgw can read
/// and runs batch sizes too small for an image of unknown attention.
///
/// The ubatch advisory (`LlamaEntry::ubatch_advisory`) is read from the
/// projector the row names (`mmproj_path`, `--mmproj`, `-mm`). A row can
/// load one lmgw never sees: `--mmproj-url`/`-mmu`, or the projector `-hf`
/// fetches beside the weights by itself. When its server then says it sees,
/// nothing is known about how llama.cpp decodes its images, so it is held
/// to what an unknown projector needs ([`crate::modelinfo::projector_ubatch_floor`]
/// on unknown attention: Gemma 4's measured image ubatch, or the row's own
/// `--image-max-tokens` when larger), batch and ubatch both. Read only when
/// the server says it sees; a row whose projector lmgw reads answers `false`
/// here and is judged by its advisory.
pub(crate) fn unread_projector_short(started: &GateFacts) -> bool {
    use crate::modelinfo::{
        projector_ubatch_floor, row_batches, row_loads_projector, ImageAttention,
    };
    if row_loads_projector(&started.params, &started.args) {
        return false;
    }
    let unknown = ImageAttention::Unknown(String::new());
    let Some((need, _)) = projector_ubatch_floor(&unknown, &started.args) else {
        return false;
    };
    let (batch, ubatch) = row_batches(&started.params, &started.args);
    batch.min(ubatch) < need
}

/// Hold `route`'s decision against container `generation` of `hold`'s model,
/// the one this attempt goes to (module doc: rechecked per attempt, only
/// ever downward). `started` is that container's start row
/// (`LocalHold::attempt_with_facts`). Nothing is read unless the route lets
/// tool images go.
pub(crate) fn recheck(
    route: &mut Route,
    hold: &LocalHold,
    generation: u64,
    started: Option<&GateFacts>,
) {
    let Some(decided) = route.upstream.llama.as_deref() else {
        return;
    };
    if !decided.tool_images.allowed() {
        return;
    }
    let Some(now) = rechecked(decided, hold.llama_at(generation).as_ref(), started) else {
        return;
    };
    let why = now
        .as_deref()
        .and_then(|l| l.tool_images.refusal())
        .unwrap_or("the server it goes to did not say whether it sees");
    tracing::warn!(
        model = %hold.model_id(),
        "tool images were decided for another container of this model than the one this \
         attempt goes to, which cannot take them: they go as placeholders ({why})"
    );
    route.upstream.llama = now;
}

/// [`recheck`]'s judgement, pure: `None` when the decision stands on the
/// container described by `entry` and `started`, else the route's new
/// `Upstream.llama` — that container's facts with what the predicate says
/// there, or `None` (today's bytes) where it has no facts. Only called on a
/// decision that lets tool images go; never returns one that lets more go.
fn rechecked(
    decided: &LlamaRoute,
    entry: Option<&LlamaEntry>,
    started: Option<&GateFacts>,
) -> Option<Option<Arc<LlamaRoute>>> {
    let Some(facts) = entry.and_then(LlamaEntry::facts) else {
        return Some(None);
    };
    let inputs = ToolImageInputs {
        vision: facts.vision,
        ubatch_advisory: entry.and_then(|e| e.ubatch_advisory.as_deref()),
        unread_projector_short: started.is_some_and(unread_projector_short),
        // Held when the decision was made, and not the container's to undo.
        guarded_bound_known: None,
        candidate_vision: None,
    };
    let now = tool_image_predicate(&inputs);
    // A webp goes only where both containers decode it: the count held it
    // only if the first did, and the second has to.
    let webp_lost = decided.facts.video == Some(true) && facts.video != Some(true);
    if now.allowed() && !webp_lost {
        return None;
    }
    Some(Some(Arc::new(LlamaRoute {
        facts: facts.clone(),
        tool_images: now,
    })))
}

/// `route` carrying the send's decision (`Upstream.llama`): the route
/// itself when there is none to carry and it carries none.
pub(crate) fn on_route<'r>(route: &'r Route, llama: Option<&Arc<LlamaRoute>>) -> Cow<'r, Route> {
    if llama.is_none() && route.upstream.llama.is_none() {
        return Cow::Borrowed(route);
    }
    let mut decided = route.clone();
    decided.upstream.llama = llama.cloned();
    Cow::Owned(decided)
}

/// How many tool-result images in `ir` pass the format check, and so go as
/// images where the decision allows it: exactly the ones the llama.cpp
/// egress's renderer sends (`tool_results::LlamaToolResults`). `webp` is
/// whether the server decodes webp (`/props` said `video: true`).
///
/// Only `tool` messages count, as only they are rendered as tool results.
/// What this adds to the count, at the row's per-image bound, is the fit's
/// and `count_on_template`'s to decide: only when the decision holds.
pub fn tool_media(ir: &ChatRequest, webp: bool) -> u64 {
    let images = ir
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .flat_map(|m| &m.content)
        .filter_map(|p| match p {
            ContentPart::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .flatten()
        .filter(|b| match b {
            ToolResultBlock::Image { mime, data } => image_format(mime, data, webp).is_ok(),
            _ => false,
        })
        .count();
    u64::try_from(images).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
