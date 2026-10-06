//! A request's images on a fallback that cannot see them (the owner's ruling,
//! 2026-10-06: a configured fallback is always used, with no exception by
//! content; only capability keeps content from a route).
//!
//! When the gate hands a request to a configured fallback — the GPU hold's
//! or a benchmark lease's swap, §4.7's outside-VRAM swap, a ladder climb's,
//! a candidate alias's ([`Route::fallback`]) — and that fallback's exposed
//! capabilities say `vision: false`, every image the request carries goes to
//! it as a placeholder, with a WARN: a user message's image part and a tool
//! result's image block alike. The placeholder is the one the llama.cpp egress puts
//! where a tool image may not go ([`crate::ir::image_placeholder`], llama
//! egress design §8.2); its reason names no model ([`PLACEHOLDER_REASON`]),
//! since the text goes to a provider that never heard lmgw's aliases.
//! *Changed 2026-10-06:* until then admission waited for the local model
//! instead (candidate-aliases §12 entry 43), and the hold's swap sent the
//! images as they were.
//!
//! - **Capability only.** `vision: false` → placeholders; `vision: true` →
//!   the images go. Unknown (`None`, absent means unknown) means today, as
//!   the llama egress's decision 14 rules: the images go, and the upstream
//!   answers for itself.
//! - **Only a fallback.** A route the client named gets the request as it
//!   was sent: lmgw adapts content only for a model it chose itself.
//! - **A candidate alias's fallback too.** An image request to an alias
//!   without Vision is refused (§4.6); with Vision enabled, a fallback that
//!   lacks it no longer counts as none (changed 2026-10-06): it answers, and
//!   gets the images as placeholders here.
//! - **Decided once per send** ([`fit`], at the top of `gate::fit_chat`,
//!   which every chat send runs): `/v1/chat/completions`, `/v1/messages`,
//!   `/v1/responses`' turns, the Chat and agent runs all send what it
//!   returns. (MCP sampling goes the same way, but carries no image: its
//!   ingress keeps a message's text only.) A caller that sends one conversation many times (a
//!   tool loop: the Chat's, `/v1/responses`') puts the placeholders in
//!   itself first, through [`Announced`], so the WARN names each image once
//!   per turn; `fit` then finds none left. The Chat also swaps a PDF sent
//!   as page images for its text first (`web::chat_turn::blind`).
//!   `/v1/responses`' native passthrough forwards the client's own body, so
//!   it asks [`blind_fallback`] and puts the same placeholders into that
//!   body (`responses::unseen`); `/v1/messages/count_tokens` counts what the
//!   send would carry ([`counted`]), on an Anthropic route by the same
//!   rewrite of the client's body (`proxy::count_messages::unseen`).
//! - **What the client learns.** On `/v1/chat/completions`, `/v1/messages`
//!   and `/v1/responses`, `x-lmgw-images-omitted: <n>` next to
//!   `x-lmgw-fallback` (the lease's [`crate::gate::TurnLease::images_omitted`]);
//!   the Chat says it on the reply, and stores it (`images_note`).
//!
//! What a fallback is asked costs nothing on a request without images, and
//! nothing on a route the client named: the capability lookup runs only when
//! both hold.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use crate::config::Route;
use crate::ir::{
    image_note, image_placeholder, tool_image_note, tool_image_placeholder, ChatRequest,
    ContentPart, ToolResultBlock,
};
use crate::state::SharedState;

/// Why an image is not sent, as each placeholder says it to the model. It
/// names no alias: the model reading it needs no more, and lmgw's names are
/// the owner's, not the provider's. The WARN, `x-lmgw-fallback` and the
/// Chat's note name them.
pub const PLACEHOLDER_REASON: &str = "the answering model cannot see images";

/// A fallback that cannot see, answering a request ([`blind_fallback`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unseen {
    /// The fallback that answers ([`Route::fallback`]).
    pub(crate) fallback: Arc<str>,
    /// The name the request asked for.
    pub(crate) requested: String,
}

impl Unseen {
    /// The request row's marker for `n` images it got as placeholders
    /// (`request_logs.degraded`, [`crate::degraded`]).
    pub(crate) fn marker(&self, n: usize) -> String {
        crate::degraded::lacks(
            &self.fallback,
            true,
            "vision",
            &crate::degraded::images(n, "a placeholder", "placeholders"),
        )
    }
}

/// What a send left out ([`fit`]): how many images, and its request row's
/// marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Omitted {
    pub(crate) count: usize,
    pub(crate) marker: String,
}

/// Whether `ir` carries an image anywhere: a message's image part, or an
/// image block of a tool result.
pub fn carries_images(ir: &ChatRequest) -> bool {
    count_images(ir) > 0
}

/// How many images `ir` carries ([`carries_images`]).
pub fn count_images(ir: &ChatRequest) -> usize {
    ir.messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|p| match p {
            ContentPart::Image { .. } => 1,
            ContentPart::ToolResult { content, .. } => content
                .iter()
                .filter(|b| matches!(b, ToolResultBlock::Image { .. }))
                .count(),
            _ => 0,
        })
        .sum()
}

/// Whether `route` is a fallback that cannot see (module doc), whatever the
/// request carries: `Some` only when `route` is a fallback and its exposed
/// capabilities say `vision: false`. Read live, from the catalog cache.
/// `requested` is the name the request asked for.
pub(crate) async fn blind_fallback(
    state: &SharedState,
    route: &Route,
    requested: &str,
) -> Option<Unseen> {
    let fallback = route.fallback.as_ref()?;
    let vision = crate::capabilities::exposed::exposed_entry(state, fallback)
        .await
        .and_then(|e| e.capabilities)
        .and_then(|c| c.vision);
    (vision == Some(false)).then(|| Unseen {
        fallback: Arc::clone(fallback),
        requested: requested.to_string(),
    })
}

/// [`blind_fallback`] for a request that carries images ([`carries_images`]);
/// `None` without one, and then nothing is looked up.
pub(crate) async fn decide(state: &SharedState, route: &Route, ir: &ChatRequest) -> Option<Unseen> {
    if !carries_images(ir) {
        return None;
    }
    blind_fallback(state, route, &ir.model_alias).await
}

/// `ir` with every image replaced by its placeholder (module doc), and what
/// each placeholder stands for, in request order, for the WARN ([`warn`]).
pub(crate) fn without_images(ir: &ChatRequest) -> (ChatRequest, Vec<String>) {
    let mut dropped = Vec::new();
    let mut out = ir.clone();
    for part in out.messages.iter_mut().flat_map(|m| &mut m.content) {
        match part {
            ContentPart::Image { mime, source } => {
                dropped.push(image_note(mime, source));
                *part = ContentPart::text(image_placeholder(mime, source, PLACEHOLDER_REASON));
            }
            ContentPart::ToolResult { content, .. } => {
                for block in content.iter_mut() {
                    if let ToolResultBlock::Image { mime, data } = block {
                        dropped.push(tool_image_note(mime, data));
                        *block = ToolResultBlock::text(tool_image_placeholder(
                            mime,
                            data,
                            PLACEHOLDER_REASON,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    (out, dropped)
}

/// The WARN a send to a fallback that cannot see logs: the fallback, the
/// model asked for, and what each placeholder stands for (`dropped`).
pub(crate) fn warn(unseen: &Unseen, dropped: &[String]) {
    tracing::warn!(
        fallback = %unseen.fallback,
        "images not sent to the fallback '{}' answering for '{}', which cannot see images — \
         replaced with placeholders: {}",
        unseen.fallback,
        unseen.requested,
        dropped.join("; ")
    );
}

/// What a send on `route` carries of `ir` ([`decide`], then
/// [`without_images`], with the WARN), and what it left out: `ir` itself,
/// and nothing, unless a fallback that cannot see answers it.
pub(crate) async fn fit<'a>(
    state: &SharedState,
    route: &Route,
    ir: &'a ChatRequest,
) -> (Cow<'a, ChatRequest>, Option<Omitted>) {
    match decide(state, route, ir).await {
        Some(unseen) => {
            let (out, dropped) = without_images(ir);
            warn(&unseen, &dropped);
            let omitted = Omitted {
                count: dropped.len(),
                marker: unseen.marker(dropped.len()),
            };
            (Cow::Owned(out), Some(omitted))
        }
        None => (Cow::Borrowed(ir), None),
    }
}

/// [`fit`] for a count, which sends nothing: the same request, no WARN.
pub(crate) async fn counted<'a>(
    state: &SharedState,
    route: &Route,
    ir: &'a ChatRequest,
) -> Cow<'a, ChatRequest> {
    match decide(state, route, ir).await {
        Some(_) => Cow::Owned(without_images(ir).0),
        None => Cow::Borrowed(ir),
    }
}

/// The placeholders of one turn that is sent more than once — a tool loop,
/// whose every call carries the conversation so far (module doc): the WARN
/// names only the images the turn has not named yet. A conversation only
/// grows at its end, and [`without_images`] lists images in request order,
/// so the images already named are the list's head.
#[derive(Debug, Default)]
pub(crate) struct Announced(Mutex<usize>);

impl Announced {
    /// `ir` with every image as its placeholder, for `unseen`, and how many
    /// it left out; the WARN names those past the ones named before.
    pub(crate) fn without_images(&self, ir: &ChatRequest, unseen: &Unseen) -> (ChatRequest, usize) {
        let (out, dropped) = without_images(ir);
        let mut named = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if dropped.len() > *named {
            warn(unseen, &dropped[*named..]);
            *named = dropped.len();
        }
        (out, dropped.len())
    }

    /// How many images this turn has left out so far.
    pub(crate) fn count(&self) -> usize {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests;
