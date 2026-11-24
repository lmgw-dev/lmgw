//! One call to a chosen model (design §7.5, D6), a page's to the
//! page-reading model or a picture's to the picture-description model
//! ([`What`]; `super::Purpose` names the model in its words): made as
//! the calling principal through the gateway's one in-process send
//! ([`crate::proxy::sample_once_dropping`]), so it is a model call like any
//! other: the gate's per-send fit, the dead-container retry, a ladder's
//! climb, and one usage row under the caller's key, labelled
//! by its purpose (`documents-page`, `documents-picture`), holding counts
//! and codes and no document content. A
//! call dropped before its answer (the client gone, `document_timeout`, a
//! cancelled job) leaves the in-flight gauge closed and a `499` row.
//!
//! **The key's policy, each call** ([`allowed`]). The key is read again by
//! its identity (a job outlives the request that started it: a key
//! disabled, rotated or deleted since stops reading pages), then its expiry,
//! its scope and its budget for the chosen model
//! ([`crate::policy::check_alias`]), and its tokens-per-minute window
//! ([`crate::policy::PolicyGate::check_call`]). The call is not counted
//! against the key's requests-per-minute window: the conversion is the
//! request the key made, and its pages are that request's work, as a
//! `/v1/responses` run's turns are.
//!
//! **What is sent depends on who answers** ([`OnRoute`]). The chosen model
//! itself is asked in the form the setting says (docling's DocTags prompt,
//! or lmgw's Markdown prompts). Any fallback that answers for it, the one
//! the request opened or one a send was handed on to, is asked with lmgw's
//! Markdown prompts and read as Markdown: a general model knows nothing of
//! DocTags. A fallback that takes no pictures is not asked at all: the gate
//! would put a text placeholder where the page's image was
//! (`gate::fallback_images`), and whatever such a model answered would not
//! be the page.
//!
//! **Sampling.** Temperature 0: a transcription has one right answer (and
//! it is what the DocTags model was measured with, design §14 WP0a). A
//! model's own row defaults apply to everything else.
//!
//! **A whole answer or none** ([`judged`]). Only an answer the model ended
//! itself is a page. `finish_reason: length` is a page that ran to its
//! bound, a runaway or a page longer than the model can write: a failed
//! page, never half a page passed off as whole. So are a refusal (OpenAI's
//! `message.refusal` arrives with `finish_reason: stop`), a filtered
//! answer, a tool call, any other finish reason (Anthropic's
//! `model_context_window_exceeded`, `pause_turn`), and an answer without
//! text.

use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;

use super::budget::{self, Bound};
use super::prompts;
use super::select::Ask;
use super::session::Open;
use super::{Chosen, Purpose};
use crate::config::Route;
use crate::erased::Erased;
use crate::error::GatewayError;
use crate::ir::{
    ChatRequest, ContentPart, FinishReason, ImageSource, Message, Params, Role, Usage,
};
use crate::proxy::{Dropped, FallbackNote, PerRoute};

/// How a page's call ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Asked {
    /// The model's whole answer, the model that gave it when the send was
    /// handed on from the route the request had opened, and whether that
    /// model was asked for DocTags.
    Text {
        text: String,
        handed_to: Option<String>,
        doctags: bool,
    },
    /// The hold took the model while the call was on its way.
    Held(String),
    /// The send was handed on to a fallback that takes no pictures: its
    /// name. Nothing was sent to it.
    Blind(String),
    /// The key's policy refused the call where it was sent (a budget or a
    /// price checked at the send).
    NotAllowed(String),
    Failed(String),
}

/// Whether the caller's key may still call the chosen model now (the
/// check every model call of a conversion asks, `key_check.rs`).
pub(crate) async fn allowed(chosen: &Chosen) -> Result<(), GatewayError> {
    super::super::key_check::may_call(&chosen.state, &chosen.ctx, &chosen.alias).await
}

/// What a page is asked with: how ([`Ask`]) and the text the tools read of
/// it, which a Markdown prompt for a page with text carries.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageAsk<'a> {
    pub ask: Ask,
    pub text: &'a str,
}

impl PageAsk<'_> {
    fn prompt(&self, doctags: bool) -> String {
        prompts::prompt(self.ask, doctags, self.text)
    }
}

/// What a call asks about its image.
#[derive(Clone, Copy)]
pub(crate) enum What<'a> {
    /// A page, in the form the answering model is asked for.
    Page(PageAsk<'a>),
    /// A picture: what makes its whole prompt (the same on every route),
    /// asked only once the call is made, so a request's pictures never
    /// hold their prompts all at once (`describe::stage`).
    Picture(&'a (dyn Fn() -> String + Send + Sync)),
}

impl std::fmt::Debug for What<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Page(p) => f.debug_tuple("Page").field(p).finish(),
            Self::Picture(_) => f.write_str("Picture"),
        }
    }
}

impl What<'_> {
    fn prompt(&self, doctags: bool) -> String {
        match self {
            Self::Page(p) => p.prompt(doctags),
            Self::Picture(make) => make(),
        }
    }
}

/// Whether the model on `route` is asked for DocTags: only the chosen model
/// itself is, never a fallback answering for it.
fn doctags_on(chosen: &Chosen, route: &Route) -> bool {
    chosen.doctags && route.fallback.is_none()
}

fn request(alias: &str, image: &[u8], mime: &str, prompt: String) -> ChatRequest {
    ChatRequest {
        model_alias: alias.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: vec![
                ContentPart::Image {
                    mime: mime.into(),
                    source: ImageSource::Base64 {
                        data: base64::engine::general_purpose::STANDARD.encode(image),
                    },
                    cache_control: None,
                },
                ContentPart::text(prompt),
            ],
        }],
        params: Params {
            temperature: Some(0.0),
            ..Default::default()
        },
        tools: Vec::new(),
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        response_format: None,
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
        context_management: None,
        compaction: None,
        thinking_display: None,
        cache_control: None,
    }
}

#[derive(Default)]
struct Sent {
    /// The route last asked was asked for DocTags.
    doctags: bool,
    /// A fallback that takes no pictures was to answer: its name.
    blind: Option<String>,
}

/// What a page's call sends on each route it lands on (module doc): the
/// route the request opened, and every one the gate hands the send on to.
struct OnRoute<'a> {
    chosen: &'a Chosen,
    what: What<'a>,
    sent: Mutex<Sent>,
}

impl OnRoute<'_> {
    fn sent(&self) -> std::sync::MutexGuard<'_, Sent> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl PerRoute for OnRoute<'_> {
    fn request<'a>(
        &'a self,
        route: &'a Route,
        _hold: Option<&'a crate::vram::LocalHold>,
        rerouted: Option<&'a crate::gate::GateHeaders>,
        ir: &'a ChatRequest,
    ) -> Erased<'a, Result<Option<ChatRequest>, GatewayError>> {
        Box::pin(async move {
            let chosen = self.chosen;
            let blind =
                crate::gate::fallback_images::blind_fallback(&chosen.state, route, &chosen.alias)
                    .await;
            if let Some(unseen) = blind {
                let name = unseen.fallback.to_string();
                let refusal = match chosen.purpose {
                    Purpose::Pages => GatewayError::Unsupported(format!(
                        "'{name}' answers for the page-reading model and takes no pictures, so \
                         a page's image cannot be sent to it"
                    )),
                    Purpose::Pictures => GatewayError::Unsupported(format!(
                        "'{name}' answers for the picture-description model and takes no \
                         pictures, so a picture cannot be sent to it"
                    )),
                };
                self.sent().blind = Some(name);
                return Err(refusal);
            }
            let doctags = doctags_on(chosen, route);
            self.sent().doctags = doctags;
            // The route the request opened: `ir` was built for it.
            if rerouted.is_none() {
                return Ok(None);
            }
            // A route the gate chose for this send: its own prompt, and no
            // `max_tokens`, as the bound was counted for the route it left.
            let mut own = ir.clone();
            own.params.max_tokens = None;
            if let Some(m) = own.messages.first_mut() {
                m.content.retain(|p| matches!(p, ContentPart::Image { .. }));
                m.content.push(ContentPart::text(self.what.prompt(doctags)));
            }
            Ok(Some(own))
        })
    }
}

/// Ask the model on `open` about `image` (of the type `mime`): how it
/// ended. `sent` is told when the call goes out (one that did has its usage
/// row, also when it is dropped from here on).
pub(crate) async fn ask(
    chosen: &Chosen,
    open: &Open,
    image: Vec<u8>,
    mime: &str,
    what: What<'_>,
    sent: impl FnOnce(),
) -> Asked {
    let state = &chosen.state;
    let doctags = doctags_on(chosen, &open.opened.route);
    let mut ir = request(&chosen.alias, &image, mime, what.prompt(doctags));
    drop(image);
    let bound = match budget::of(chosen, open, &ir).await {
        Ok(b) => b,
        Err(e) => return Asked::Failed(format!("its prompt could not be sized: {e}")),
    };
    ir.params.max_tokens = bound.max_tokens();
    // The gateway's own bound on an in-process model call, a visible
    // setting (Settings › Network & access), as the knowledge bases' page
    // reads use it; the file's own deadline ends the call by dropping it.
    let deadline = Duration::from_secs(state.snapshot().settings.responses_timeout_seconds.max(1));
    let note = FallbackNote::default();
    let on_route = OnRoute {
        chosen,
        what,
        sent: Mutex::new(Sent {
            doctags,
            blind: None,
        }),
    };
    sent();
    // Counted where a cut part of the conversion would lose it
    // (`convert::sent`).
    crate::extract::convert::sent::record(lmgw_api_types::documents::model_help::ModelCalls {
        alias: open.answered_by.clone(),
        answered_for: open.fallback.then(|| chosen.alias.clone()),
        calls: 1,
        parts_read: 0,
        pictures_described: 0,
        files_transcribed: 0,
        in_flight_bound: open.bound,
        bound_from: open.bound_from,
    });
    let done = crate::proxy::sample_once_dropping(
        state,
        open.opened.hold.as_ref(),
        &open.opened.route,
        open.opened.headers.fallback_reason(),
        &ir,
        chosen.purpose.proto(),
        chosen.ctx.key_ref(),
        deadline,
        Some(&note),
        Some((&on_route, None)),
        Dropped::Row,
    )
    .await;
    let handed_to = note
        .take()
        .and_then(|chain| chain.last().map(|(name, _)| name.to_string()));
    let Sent { doctags, blind } = std::mem::take(&mut *on_route.sent());
    if let Some(fallback) = blind {
        return Asked::Blind(fallback);
    }
    match done {
        Ok(c) => judged_as(
            chosen.purpose,
            c.finish_reason,
            &c.content,
            c.usage,
            bound,
            handed_to,
            doctags,
        ),
        Err(e @ (GatewayError::GpuHold { .. } | GatewayError::GpuBenchmark { .. })) => {
            Asked::Held(e.to_string())
        }
        Err(
            e @ (GatewayError::KeyScope { .. }
            | GatewayError::KeyBudget { .. }
            | GatewayError::BudgetUnpriced { .. }
            | GatewayError::KeyRate { .. }
            | GatewayError::KeyExpired { .. }),
        ) => Asked::NotAllowed(e.to_string()),
        Err(e) => Asked::Failed(e.to_string()),
    }
}

/// A finish reason an upstream named, as a warning may quote it: a plain
/// word (it is the upstream's text), or that it was none.
fn reason_said(reason: &str) -> String {
    let word = !reason.is_empty()
        && reason.len() <= 64
        && reason
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if word {
        format!("the finish reason '{reason}'")
    } else {
        "a finish reason lmgw does not know".to_string()
    }
}

/// [`judged_as`] for a page.
#[cfg(test)]
fn judged(
    finish: FinishReason,
    content: &[ContentPart],
    usage: Usage,
    bound: Bound,
    handed_to: Option<String>,
    doctags: bool,
) -> Asked {
    judged_as(
        Purpose::Pages,
        finish,
        content,
        usage,
        bound,
        handed_to,
        doctags,
    )
}

/// A finished call's answer, or why it is not one, in the words of what
/// was asked (`purpose`).
fn judged_as(
    purpose: Purpose,
    finish: FinishReason,
    content: &[ContentPart],
    usage: Usage,
    bound: Bound,
    handed_to: Option<String>,
    doctags: bool,
) -> Asked {
    let picture = purpose == Purpose::Pictures;
    match &finish {
        FinishReason::Stop | FinishReason::StopSequence(_) => {}
        FinishReason::Length => {
            let wrote = usage
                .completion_tokens
                .map(|n| format!(" after {n} tokens"))
                .unwrap_or_default();
            if picture {
                return Asked::Failed(format!(
                    "its answer ran to the token limit{wrote} without ending ({}), so it is not \
                     a whole description: a model repeating itself",
                    bound.said_for(purpose.image())
                ));
            }
            return Asked::Failed(format!(
                "its answer ran to the token limit{wrote} without ending ({}), so it is not a \
                 whole page: a model repeating itself, or a page with more text than the model \
                 can write",
                bound.said()
            ));
        }
        FinishReason::ContentFilter => {
            return Asked::Failed("the model's provider filtered its answer".to_string())
        }
        FinishReason::ToolUse if picture => {
            return Asked::Failed(
                "the model answered with a tool call instead of a description".to_string(),
            )
        }
        FinishReason::ToolUse => {
            return Asked::Failed(
                "the model answered with a tool call instead of the page's text".to_string(),
            )
        }
        FinishReason::Other(reason) if picture => {
            return Asked::Failed(format!(
                "the model's answer ended with {}, not with the end of its answer, so it is \
                 not a whole description",
                reason_said(reason)
            ))
        }
        FinishReason::Other(reason) => {
            return Asked::Failed(format!(
                "the model's answer ended with {}, not with the end of the page, so it is not \
                 a whole page",
                reason_said(reason)
            ))
        }
    }
    // A refusal is the model's words about the request, never the page's.
    let refused = |p: &ContentPart| matches!(p, ContentPart::Refusal { .. });
    if content.iter().any(refused) {
        return Asked::Failed(if picture {
            "the model refused to describe the picture (it answered with a refusal)".to_string()
        } else {
            "the model refused to read the page (it answered with a refusal)".to_string()
        });
    }
    let text: String = content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if text.trim().is_empty() {
        return Asked::Failed("the model answered with no text".to_string());
    }
    Asked::Text {
        text,
        handed_to,
        doctags,
    }
}

#[cfg(test)]
mod tests;
