//! Whether the caller's prompt leaves the picture-description model room for
//! an answer (design §9: every bound from a real value). `picture_prompt`
//! has no size limit of its own besides the request body (`max_body_mb`):
//! the real bound is the answering model's context. So once the route is
//! open, the prompt is counted against that context, once per route the
//! request opens: a prompt that by itself fills the context makes no
//! picture call at all, and the file's one note says so with both numbers.
//!
//! - **The count** is the gateway's own (`proxy::count_text_route`, what
//!   `/v1/count_tokens` counts with): the running llama-server's tokenizer
//!   for a local model, the provider's count for Anthropic and Gemini, and
//!   tiktoken (`o200k_base` where lmgw does not know the model's own) for an
//!   OpenAI-shaped upstream, run in a slot of the tool pool off the runtime
//!   threads.
//! - **A prompt of fewer bytes than the context's tokens** is not counted:
//!   every tokenizer lmgw counts with makes at most about one token per byte,
//!   so it leaves room by construction, and each call's own bound
//!   (`model_help::budget`) still counts the whole message.
//! - **Nothing is judged** where the context is not known, or the count
//!   cannot be made: the calls go out and each meets its own bound.

use std::sync::{Arc, Weak};

use tokio::sync::Mutex;

use super::Describe;
use crate::config::Protocol;
use crate::extract::convert::model_help::call;
use crate::extract::convert::model_help::session::{Keep, Open};
use crate::extract::convert::model_help::Chosen;
use crate::extract::convert::pool;

/// A request's verdict on its prompt, for the route it was judged on.
#[derive(Default)]
pub(crate) struct Room {
    judged: Mutex<Option<(Weak<Open>, Option<String>)>>,
}

impl Room {
    /// Why no picture call can be made with `d`'s prompt on the route the
    /// request has open now (opened for `keep`'s stage), or `None`. A key
    /// that may not call the model, and a route that cannot be opened, are
    /// left to each picture's call, which names them.
    pub(super) async fn refused(&self, d: &Describe, keep: &Keep) -> Option<String> {
        let chosen = &*d.chosen;
        call::allowed(chosen).await.ok()?;
        let open = chosen.session.current(chosen, keep).await.ok()?;
        let mut judged = self.judged.lock().await;
        if let Some((on, said)) = judged.as_ref() {
            if std::ptr::eq(on.as_ptr(), Arc::as_ptr(&open)) {
                return said.clone();
            }
        }
        let said = judge(chosen, &open, &d.prompt).await;
        *judged = Some((Arc::downgrade(&open), said.clone()));
        said
    }
}

/// The refusal for `prompt` on `open`, when it fills the model's context.
async fn judge(chosen: &Chosen, open: &Open, prompt: &Arc<str>) -> Option<String> {
    let context = open.context?;
    if (prompt.len() as u64) < context {
        return None;
    }
    let (tokens, how) = count(chosen, open, prompt).await?;
    (tokens >= context).then(|| {
        format!(
            "picture_prompt comes to {tokens} tokens ({how}), which leaves no room for an \
             answer in the context of {context} tokens of '{}', the model that answers",
            open.answered_by
        )
    })
}

/// `prompt`'s tokens on `open`'s route, and how they were counted.
async fn count(chosen: &Chosen, open: &Open, prompt: &Arc<str>) -> Option<(u64, &'static str)> {
    let route = &open.opened.route;
    match route.upstream.protocol {
        Protocol::Openai | Protocol::OpenaiCompatible => {
            let (model, text) = (route.upstream_model.clone(), prompt.clone());
            let (n, guessed) = pool::blocking(move || {
                crate::egress::openai_compatible::tiktoken_count(&model, &text)
            })
            .await
            .ok()?;
            Some((
                n,
                if guessed {
                    "counted with tiktoken's o200k_base, an estimate"
                } else {
                    "counted with the model's tiktoken encoding"
                },
            ))
        }
        Protocol::Anthropic | Protocol::Gemini | Protocol::LlamaCpp => {
            let c = crate::proxy::count_text_route(
                &chosen.state,
                prompt,
                route,
                open.opened.hold.as_ref(),
            )
            .await
            .ok()?;
            Some((c.tokens, "counted by the model's own tokenizer"))
        }
    }
}
