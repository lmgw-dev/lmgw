//! A page call's `max_tokens`, from the answering model's real values
//! (design §9: "the alias's real context minus the image's tokens"; never
//! a guessed constant).
//!
//! - **A local llama.cpp model** (the request holds its container): the
//!   prompt is counted on the running server exactly as the gate counts a
//!   guarded model's (`/apply-template` on the body that will be sent, then
//!   `/tokenize`), plus the projector's per-image bound; `max_tokens` is the
//!   slot's context less that. Where lmgw has no bound for the projector's
//!   images ([`image_token_bound`](crate::gate::image_token_bound) says so),
//!   the image's tokens are not known: `max_tokens` is then the context less
//!   the counted text alone, and the server ends an answer that would pass
//!   its context.
//! - **Any other model**: its tokenizer and its image's tokens are not
//!   lmgw's to count, so the bound is the model's own largest answer, where
//!   its catalog says one.
//! - **An llama-server lmgw does not run** (an upstream of that kind)
//!   without such a number: its context. llama-server takes a `max_tokens`
//!   larger than the room left and ends the answer at its context.
//! - **Anything else** (a cloud or generic upstream whose largest answer
//!   lmgw does not know): no `max_tokens` is sent, and the model's own bound
//!   applies. The context alone is no bound there: prompt plus `max_tokens`
//!   would pass it by construction, and such upstreams refuse the request
//!   for it (OpenAI: larger than the model's output limit; vLLM-style
//!   servers: larger than the context), failing every page where sending
//!   none works.
//!
//! Whatever the bound, an answer that ends at it (`finish_reason: length`)
//! is a failed page ([`super::call`]), and the page's warning names the
//! bound and where it came from ([`Bound::said`]).

use crate::config::UpstreamKind;
use crate::error::GatewayError;
use crate::gate::{count_chat_prompt, image_token_bound, MediaParts};
use crate::ir::{ChatRequest, Params};

use super::session::Open;
use super::Chosen;

/// The bound a page call was sent with, and what it was derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bound {
    /// Context less the counted prompt (text, and the image when
    /// `image_counted`).
    Counted {
        max_tokens: u32,
        context: u64,
        prompt: u64,
        image_counted: bool,
    },
    /// The model's largest answer, from its catalog.
    ModelMaxOutput(u32),
    /// An llama-server's context: nothing of the prompt could be counted,
    /// and the server ends an answer at its context.
    Context(u32),
    /// lmgw cannot count the prompt against a known output limit: none sent.
    Unknown,
}

impl Bound {
    pub(crate) fn max_tokens(self) -> Option<u32> {
        match self {
            Self::Counted { max_tokens, .. } => Some(max_tokens),
            Self::ModelMaxOutput(n) | Self::Context(n) => Some(n),
            Self::Unknown => None,
        }
    }

    /// In words, for the warning of a page that ran into it.
    pub(crate) fn said(self) -> String {
        self.said_for("page image")
    }

    /// In words, the image sent named `image` ("page image", "picture").
    pub(crate) fn said_for(self, image: &str) -> String {
        match self {
            Self::Counted {
                max_tokens,
                context,
                prompt,
                image_counted,
            } => format!(
                "max_tokens {max_tokens}: the model's context of {context} tokens less the \
                 prompt's {prompt}{}",
                if image_counted {
                    format!(", the {image} included")
                } else {
                    format!(" (the {image}'s tokens are not known for this model)")
                }
            ),
            Self::ModelMaxOutput(n) => {
                format!("max_tokens {n}: the model's largest answer, from its catalog")
            }
            Self::Context(n) => format!("max_tokens {n}: the llama-server's context"),
            Self::Unknown => "the model's own limit: lmgw sent no max_tokens, as it does not \
                              know this model's largest answer"
                .to_string(),
        }
    }
}

fn clamp(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The bound for `ir` (one page image and its prompt) on `open`.
pub(crate) async fn of(
    chosen: &Chosen,
    open: &Open,
    ir: &ChatRequest,
) -> Result<Bound, GatewayError> {
    let route = &open.opened.route;
    let started = open.opened.hold.as_ref().and_then(|h| h.gate_facts());
    if let (Some(hold), Some(facts), Some(context)) =
        (open.opened.hold.as_ref(), started, open.context)
    {
        let image = image_token_bound(&facts.models_dir, facts.projector_row())
            .await
            .ok()
            .flatten();
        let http = &chosen.state.http;
        let params = Params::default().with_defaults(&route.param_defaults);
        let (params, image) = (&params, &image);
        let count = crate::gate::on_running_server(route, hold, move |root| {
            let attempt = hold.on_attempt(route);
            let body = crate::egress::llama_cpp::chat_body(
                ir,
                &route.upstream_model,
                params,
                false,
                &attempt.upstream,
            );
            // Without a bound for the image the text alone is counted.
            let media = MediaParts {
                images: u64::from(image.is_some()),
                audio: 0,
            };
            async move { count_chat_prompt(http, &root, &body, media, image.clone()).await }
        })
        .await?;
        let left = context.saturating_sub(count.total);
        if left == 0 {
            return Err(GatewayError::BadRequest(format!(
                "the {}'s prompt comes to {} tokens, which leaves no room for an answer in the \
                 model's context of {context}",
                chosen.purpose.noun(),
                count.total
            )));
        }
        return Ok(Bound::Counted {
            max_tokens: clamp(left),
            context,
            prompt: count.total,
            image_counted: image.is_some(),
        });
    }
    Ok(uncounted(
        open.max_output,
        open.context,
        route.upstream.kind == UpstreamKind::LlamaServer,
    ))
}

/// The bound on a route whose prompt lmgw cannot count (module doc).
fn uncounted(max_output: Option<u64>, context: Option<u64>, llama_server: bool) -> Bound {
    match (max_output, context) {
        (Some(out), _) => Bound::ModelMaxOutput(clamp(out)),
        (None, Some(context)) if llama_server => Bound::Context(clamp(context)),
        _ => Bound::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bound_says_where_it_comes_from() {
        let b = Bound::Counted {
            max_tokens: 7050,
            context: 8192,
            prompt: 1142,
            image_counted: true,
        };
        assert_eq!(b.max_tokens(), Some(7050));
        assert!(b
            .said()
            .contains("context of 8192 tokens less the prompt's 1142"));
        assert!(Bound::ModelMaxOutput(4096)
            .said()
            .contains("largest answer"));
        assert_eq!(Bound::Unknown.max_tokens(), None);
        assert_eq!(clamp(u64::MAX), u32::MAX);
    }

    /// S10: a context is a bound only where the server ends an answer at
    /// it; elsewhere prompt + max_tokens would pass it and be refused.
    #[test]
    fn a_context_alone_bounds_an_llama_server_and_nothing_else() {
        assert_eq!(uncounted(None, Some(128_000), false), Bound::Unknown);
        assert_eq!(uncounted(None, Some(8192), true), Bound::Context(8192));
        for llama in [false, true] {
            assert_eq!(
                uncounted(Some(4096), Some(128_000), llama),
                Bound::ModelMaxOutput(4096)
            );
            assert_eq!(uncounted(None, None, llama), Bound::Unknown);
        }
        assert!(Bound::Unknown.said().contains("no max_tokens"));
    }
}
