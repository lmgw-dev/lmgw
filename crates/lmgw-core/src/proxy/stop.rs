//! A consumer's cooperative stop for one streamed in-process call (realtime
//! design §4.3, §11).
//!
//! A realtime cancel used to abort the task making the model call, which
//! dropped [`stream_once_on`](super::stream_once_on) wherever it stood: no
//! `request_logs` row and no tokens or spend counted — a client could take a
//! whole answer budget-free by cancelling at its end — and an abort landing
//! inside the row's write left the in-flight gauge counting a call that was
//! over. A stop is cooperative instead: the consumer raises it, the call ends
//! at its next await — the gate's wait, the wait for the first byte, the wait
//! for the next chunk — and still writes its row.
//!
//! A stopped call's row says status 200 and `canceled`, as the public
//! relay's rows do for a client that went away — wherever the stop hit, so
//! a stop is one kind of row (WP1c review #5) — and carries what the call
//! cost so far: the upstream's own usage where it reported it, llama.cpp's
//! own counters where it sent them, and otherwise an estimate at ~4
//! characters a token ([`crate::extract::approx_tokens`], the `~` estimate
//! the Chat's attachment chips show). A stop after the request went out but
//! before the upstream answered estimates the prompt alone: the upstream
//! may already be working on it, and a cloud one bills for it. The row's
//! message says which numbers are estimated, since the row has no column for
//! it (realtime §11: no new columns in v1).
//!
//! On an `openai` row the estimate is not written as usage (openai egress
//! design §13.4, after the WP9 review): the platform's text is counted with
//! its own tokenizer instead ([`stopped_cost`], [`unanswered_cost`]), and
//! that count is only the budgets' lower bound. The row's tokens and cost
//! stay unknown, and its note says what the budgets counted, as on the
//! public path. Every other row keeps the estimate as its usage and cost.

use tokio::sync::watch;

use crate::error::GatewayError;
use crate::ir::{ChatRequest, Signer, Timings, Usage};

use super::estimate;

/// The consumer's half. [`Self::stop`] raises the stop, and so does dropping
/// the handle: a consumer that is gone — a realtime session that ended —
/// stops its call as surely as one that asked.
pub struct StopHandle(watch::Sender<bool>);

/// The call's half, handed to it through
/// [`DeltaSink::stop`](crate::agent::DeltaSink::stop).
#[derive(Clone)]
pub struct StopSignal(watch::Receiver<bool>);

/// A linked handle and signal, not raised.
pub fn stop_pair() -> (StopHandle, StopSignal) {
    let (tx, rx) = watch::channel(false);
    (StopHandle(tx), StopSignal(rx))
}

impl StopHandle {
    pub fn stop(&self) {
        self.0.send_replace(true);
    }
}

impl StopSignal {
    /// Whether the consumer stopped the call, or is gone.
    pub fn is_raised(&self) -> bool {
        *self.0.borrow() || self.0.has_changed().is_err()
    }

    /// Resolves once [`Self::is_raised`] would say so.
    pub async fn raised(&self) {
        let mut rx = self.0.clone();
        // `Err`: the handle was dropped — the consumer is gone, which is a
        // stop too.
        let _ = rx.wait_for(|stopped| *stopped).await;
    }
}

/// Resolves when `stop` is raised; never without one, so a `select!` arm
/// over it simply does not fire for a consumer that cannot stop a call.
pub(crate) async fn stopped(stop: Option<&StopSignal>) {
    match stop {
        Some(s) => s.raised().await,
        None => std::future::pending().await,
    }
}

/// The error of a call its consumer stopped: `canceled`, the word the rows
/// of a client that went away already use. The error says 499 ("client
/// closed request"), since the client's side ended it — the consumer asked
/// for it, so no client ever reads it — and its row says 200
/// ([`row_status`]).
pub(crate) fn canceled(message: &str) -> GatewayError {
    GatewayError::Refused {
        status: 499,
        code: "canceled",
        message: message.to_string(),
    }
}

/// Whether `e` is [`canceled`]'s: a stop, not a failure.
pub(crate) fn is_canceled(e: &GatewayError) -> bool {
    matches!(
        e,
        GatewayError::Refused {
            code: "canceled",
            ..
        }
    )
}

/// The status a call's row says for `e`: 200 for a stop, as the relay's
/// rows and a stream stopped mid-way say (module doc); the error's own
/// status otherwise.
pub(crate) fn row_status(e: &GatewayError) -> u16 {
    if is_canceled(e) {
        200
    } else {
        e.http_status().as_u16()
    }
}

/// The budgets' lower bound for an answer on `route` whose usage never
/// came, on the public path: an `openai` row counts the request and the
/// `relayed` output with the platform's tokenizer ([`count_chat`]); any
/// other cloud row (`local`: false) has no tokenizer lmgw knows, so the
/// bound is the ~4 characters a token estimate the in-process paths use
/// ([`stopped_usage`]). A local row is free and has none. Only the budgets
/// read it: the row's own cost stays unknown, with a note.
///
/// [`count_chat`]: crate::egress::openai::count_chat
pub(crate) async fn floor_count(
    route: &crate::config::Route,
    local: bool,
    ir: &ChatRequest,
    relayed: String,
) -> Option<crate::pricing::LocalCount> {
    if route.upstream.protocol == crate::config::Protocol::Openai {
        return crate::egress::openai::count_chat(route, ir, relayed).await;
    }
    if local {
        return None;
    }
    let row = estimate::anthropic_row(&route.upstream);
    let anthropic = row.as_ref();
    Some(crate::pricing::LocalCount {
        prompt: prompt_estimate(ir, anthropic),
        output: relayed.chars().count().div_ceil(4) as u64,
        estimated: true,
        uncounted: estimate::uncounted(ir, anthropic).len(),
    })
}

/// The prompt `ir` sends on its route (`anthropic`: an Anthropic row's
/// scope, [`estimate::anthropic_row`], whose row is sent only what it can
/// read), at ~4 characters a token ([`super::estimate_text`]).
fn prompt_estimate(ir: &ChatRequest, anthropic: Option<&Signer>) -> u64 {
    crate::extract::approx_tokens(&super::estimate_text(ir, anthropic)) as u64
}

/// What a call stopped after its request went out, before the upstream's
/// answer began, cost — and the row's message: the prompt it sent on its
/// route (`anthropic`, as in [`prompt_estimate`]), estimated at ~4
/// characters a token as in [`stopped_usage`] (the upstream may already be
/// working on it), and no
/// output, of which nothing was read. The message names what the estimate
/// leaves out ([`estimate::uncounted`]).
pub(crate) fn unanswered_usage(ir: &ChatRequest, anthropic: Option<&Signer>) -> (Usage, String) {
    let usage = Usage {
        prompt_tokens: Some(prompt_estimate(ir, anthropic)),
        completion_tokens: Some(0),
        ..Default::default()
    };
    let message = format!(
        "stopped by the caller before the upstream answered; prompt tokens estimated at ~4 \
         characters a token (the request had gone out){}",
        estimate::left_out_clause(&estimate::uncounted(ir, anthropic))
    );
    (usage, message)
}

/// What a stopped call's row carries: the usage it writes, the message it
/// says, and lmgw's own count of it where that bounds it instead of an
/// estimate (an `openai` row, §13.4).
pub(crate) struct StopCost {
    pub usage: Usage,
    pub message: String,
    pub count: Option<crate::pricing::LocalCount>,
}

/// [`unanswered_usage`], on `route`: on an `openai` row the prompt counted
/// with the platform's tokenizer as the budgets' lower bound, its tokens
/// left unknown.
pub(crate) async fn unanswered_cost(route: &crate::config::Route, ir: &ChatRequest) -> StopCost {
    if let Some(count) = crate::egress::openai::count_chat(route, ir, String::new()).await {
        return StopCost {
            usage: Usage::default(),
            message: "stopped by the caller before the upstream answered (the request had gone \
                      out)"
                .to_string(),
            count: Some(count),
        };
    }
    let row = estimate::anthropic_row(&route.upstream);
    let (usage, message) = unanswered_usage(ir, row.as_ref());
    StopCost {
        usage,
        message,
        count: None,
    }
}

/// [`stopped_usage`], on `route`: on an `openai` row whose upstream
/// reported no usage, the request's text and the `relayed` output counted
/// with the platform's tokenizer as the budgets' lower bound, its tokens
/// left unknown. `relayed` is read only there.
pub(crate) async fn stopped_cost(
    route: &crate::config::Route,
    reported: Usage,
    timings: Option<&Timings>,
    ir: &ChatRequest,
    produced: usize,
    relayed: String,
) -> StopCost {
    if !reported.reported() {
        if let Some(count) = crate::egress::openai::count_chat(route, ir, relayed).await {
            return StopCost {
                usage: reported,
                message: "stopped by the caller".to_string(),
                count: Some(count),
            };
        }
    }
    let row = estimate::anthropic_row(&route.upstream);
    let (usage, message) = stopped_usage(reported, timings, ir, produced, row.as_ref());
    StopCost {
        usage,
        message,
        count: None,
    }
}

/// What a stream its consumer stopped cost so far, and the row's message:
/// `reported` where the upstream said, llama.cpp's `timings` where it sent
/// them, and an estimate at ~4 characters a token otherwise — the prompt
/// from what the request sent on its route (`anthropic`, as in
/// [`prompt_estimate`], [`super::estimate_text`]), the output from the
/// `produced` characters
/// the stream delivered. The message names every estimated number, and
/// what an estimated prompt leaves out ([`estimate::uncounted`]).
pub(crate) fn stopped_usage(
    reported: Usage,
    timings: Option<&Timings>,
    ir: &ChatRequest,
    produced: usize,
    anthropic: Option<&Signer>,
) -> (Usage, String) {
    let mut u = reported;
    let mut estimated = Vec::new();
    if u.prompt_tokens.is_none() {
        u.prompt_tokens = Some(match timings {
            Some(t) => t.prompt_n + t.cache_n.unwrap_or(0),
            None => {
                estimated.push("prompt");
                prompt_estimate(ir, anthropic)
            }
        });
    }
    if u.completion_tokens.is_none() {
        u.completion_tokens = Some(match timings {
            Some(t) => t.predicted_n,
            None => {
                estimated.push("completion");
                produced.div_ceil(4) as u64
            }
        });
    }
    let message = if estimated.is_empty() {
        "stopped by the caller".to_string()
    } else {
        let left_out = match estimated.first() {
            Some(&"prompt") => estimate::left_out_clause(&estimate::uncounted(ir, anthropic)),
            _ => String::new(),
        };
        format!(
            "stopped by the caller; {} tokens estimated at ~4 characters a token (the upstream \
             reported none before the stop){left_out}",
            estimated.join(" and ")
        )
    };
    (u, message)
}

/// The characters of output one delta carries — what [`stopped_usage`]
/// estimates the completion from.
pub(crate) fn produced_chars(d: &crate::ir::StreamDelta) -> usize {
    use crate::ir::StreamDelta;
    match d {
        StreamDelta::TextDelta(t)
        | StreamDelta::RefusalDelta(t)
        | StreamDelta::ReasoningDelta(t) => t.chars().count(),
        StreamDelta::ToolCallStart { name, .. } => name.chars().count(),
        StreamDelta::ToolCallArgsDelta { fragment, .. } => fragment.chars().count(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_handle_is_a_stop() {
        let (handle, signal) = stop_pair();
        assert!(!signal.is_raised());
        drop(handle);
        assert!(signal.is_raised());
        let (handle, signal) = stop_pair();
        handle.stop();
        assert!(signal.is_raised());
    }

    fn request(text: &str) -> ChatRequest {
        ChatRequest {
            model_alias: "m".into(),
            messages: vec![crate::ir::Message::text(crate::ir::Role::User, text)],
            params: Default::default(),
            tools: Vec::new(),
            tool_choice: None,
            stream: true,
            passthrough: Default::default(),
            response_format: None,
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
            context_management: None,
            compaction: None,
            thinking_display: None,
        }
    }

    #[test]
    fn a_stop_is_a_200_row_whenever_it_hit() {
        let stop = canceled("stopped");
        assert!(is_canceled(&stop));
        assert_eq!(row_status(&stop), 200);
        assert_eq!(stop.http_status().as_u16(), 499);
        let other = GatewayError::Transport("reset".into());
        assert!(!is_canceled(&other));
        assert_eq!(row_status(&other), other.http_status().as_u16());

        // Stopped while the upstream had the request: the prompt counts.
        let (u, msg) = unanswered_usage(
            &request("abcdefgh"),
            Some(&Signer::new("https://api.anthropic.com")),
        );
        assert_eq!((u.prompt_tokens, u.completion_tokens), (Some(4), Some(0)));
        assert!(msg.contains("prompt tokens estimated"), "{msg}");
        assert!(!msg.contains("leaves out"), "{msg}");
    }

    /// On an Anthropic route the estimate counts a verbatim block's text and
    /// names what it cannot count; off one the block is not sent at all.
    #[test]
    fn an_anthropic_estimate_counts_blocks_and_names_what_it_leaves_out() {
        let ir = crate::ingress::anthropic::parse_messages_request(&serde_json::json!({
            "model": "m", "max_tokens": 9, "messages": [{"role": "user", "content": [
                {"type": "search_result", "source": "s", "title": "t",
                 "content": [{"type": "text", "text": "a long passage of forty characters...."}]},
                {"type": "document", "source": {"type": "base64",
                    "media_type": "application/pdf", "data": "JVBERi0xLjQK"}},
                {"type": "text", "text": "Sum up."},
            ]}],
        }))
        .unwrap();
        let row = Signer::new("https://api.anthropic.com");
        let anthropic = Some(&row);
        let other = None;
        let (on, msg) = unanswered_usage(&ir, anthropic);
        let (off, plain) = unanswered_usage(&ir, other);
        assert!(on.prompt_tokens > off.prompt_tokens, "{on:?} {off:?}");
        assert!(
            msg.ends_with(
                "; the estimate leaves out what lmgw cannot count locally (content.document)"
            ),
            "{msg}"
        );
        assert!(!plain.contains("leaves out"), "{plain}");
        let (_, msg) = stopped_usage(Usage::default(), None, &ir, 0, anthropic);
        assert!(msg.contains("(content.document)"), "{msg}");
    }

    #[test]
    fn reported_numbers_stay_and_missing_ones_are_estimated_and_named() {
        let ir = ChatRequest {
            model_alias: "m".into(),
            messages: vec![crate::ir::Message::text(crate::ir::Role::User, "abcdefgh")],
            params: Default::default(),
            tools: Vec::new(),
            tool_choice: None,
            stream: true,
            passthrough: Default::default(),
            response_format: None,
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
            context_management: None,
            compaction: None,
            thinking_display: None,
        };
        let row = Signer::new("https://api.anthropic.com");
        let anthropic = Some(&row);
        let (u, msg) = stopped_usage(Usage::default(), None, &ir, 9, anthropic);
        // "user: abcdefgh" is 14 characters.
        assert_eq!(u.prompt_tokens, Some(4));
        assert_eq!(u.completion_tokens, Some(3));
        assert!(
            msg.contains("prompt and completion tokens estimated"),
            "{msg}"
        );

        let reported = Usage {
            prompt_tokens: Some(40),
            ..Default::default()
        };
        let (u, msg) = stopped_usage(reported, None, &ir, 9, anthropic);
        assert_eq!(u.prompt_tokens, Some(40));
        assert!(msg.contains("completion tokens estimated") && !msg.contains("prompt"));

        let t = Timings {
            prompt_n: 10,
            prompt_ms: Some(1.0),
            prompt_per_second: Some(1.0),
            predicted_n: 7,
            predicted_ms: Some(1.0),
            predicted_per_second: Some(1.0),
            cache_n: Some(5),
            draft_n: None,
            draft_n_accepted: None,
        };
        let (u, msg) = stopped_usage(Usage::default(), Some(&t), &ir, 9, anthropic);
        assert_eq!((u.prompt_tokens, u.completion_tokens), (Some(15), Some(7)));
        assert_eq!(msg, "stopped by the caller");
    }
}
