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

use tokio::sync::watch;

use crate::error::GatewayError;
use crate::ir::{ChatRequest, Timings, Usage};

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

/// What a call stopped after its request went out, before the upstream's
/// answer began, cost — and the row's message: the prompt, estimated at ~4
/// characters a token as in [`stopped_usage`] (the upstream may already be
/// working on it), and no output, of which nothing was read.
pub(crate) fn unanswered_usage(ir: &ChatRequest) -> (Usage, String) {
    let prompt = crate::extract::approx_tokens(&super::flatten_for_count(ir)) as u64;
    let usage = Usage {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(0),
        ..Default::default()
    };
    let message = "stopped by the caller before the upstream answered; prompt tokens estimated \
                   at ~4 characters a token (the request had gone out)"
        .to_string();
    (usage, message)
}

/// What a stream its consumer stopped cost so far, and the row's message:
/// `reported` where the upstream said, llama.cpp's `timings` where it sent
/// them, and an estimate at ~4 characters a token otherwise — the prompt
/// from the request's text ([`super::flatten_for_count`]), the output from
/// the `produced` characters the stream delivered. The message names every
/// estimated number.
pub(crate) fn stopped_usage(
    reported: Usage,
    timings: Option<&Timings>,
    ir: &ChatRequest,
    produced: usize,
) -> (Usage, String) {
    let mut u = reported;
    let mut estimated = Vec::new();
    if u.prompt_tokens.is_none() {
        u.prompt_tokens = Some(match timings {
            Some(t) => t.prompt_n + t.cache_n.unwrap_or(0),
            None => {
                estimated.push("prompt");
                crate::extract::approx_tokens(&super::flatten_for_count(ir)) as u64
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
        format!(
            "stopped by the caller; {} tokens estimated at ~4 characters a token (the upstream \
             reported none before the stop)",
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
        StreamDelta::TextDelta(t) | StreamDelta::ReasoningDelta(t) => t.chars().count(),
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
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
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
        let (u, msg) = unanswered_usage(&request("abcdefgh"));
        assert_eq!((u.prompt_tokens, u.completion_tokens), (Some(4), Some(0)));
        assert!(msg.contains("prompt tokens estimated"), "{msg}");
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
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
        };
        let (u, msg) = stopped_usage(Usage::default(), None, &ir, 9);
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
        let (u, msg) = stopped_usage(reported, None, &ir, 9);
        assert_eq!(u.prompt_tokens, Some(40));
        assert!(msg.contains("completion tokens estimated") && !msg.contains("prompt"));

        let t = Timings {
            prompt_n: 10,
            prompt_ms: 1.0,
            prompt_per_second: 1.0,
            predicted_n: 7,
            predicted_ms: 1.0,
            predicted_per_second: 1.0,
            cache_n: Some(5),
            draft_n: None,
            draft_n_accepted: None,
        };
        let (u, msg) = stopped_usage(Usage::default(), Some(&t), &ir, 9);
        assert_eq!((u.prompt_tokens, u.completion_tokens), (Some(15), Some(7)));
        assert_eq!(msg, "stopped by the caller");
    }
}
