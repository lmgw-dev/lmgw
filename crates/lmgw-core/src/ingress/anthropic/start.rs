//! A Messages stream's `message_start` (gateway design §6.2).
//!
//! Anthropic's own stream opens with `message_start`, whose
//! `message.usage` holds the request's input counts: `input_tokens`, the
//! cache counters and the `cache_creation` split, and a small
//! `output_tokens`. An answer from an Anthropic-protocol upstream has them
//! in its own `message_start`, which the decoder hands on as the stream's
//! first [`StreamDelta::Usage`]; so the encoder holds its `message_start`
//! back until the first delta arrives ([`Start::Held`]), and writes that
//! usage into it. Every other upstream reports its usage only at the end:
//! its `message_start` goes out at once, with zeros ([`Start::Now`]).

use serde_json::{json, Value};

use super::usage::anthropic_usage;
use crate::ir::{StreamDelta, Usage};
use crate::sse::frame;

/// When the encoder writes `message_start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Start {
    /// At `start()`, with zeros: the upstream reports usage only at the end.
    Now,
    /// Not yet written: it waits for the first delta, which on an
    /// Anthropic-protocol upstream is the usage of its own `message_start`.
    Held,
    /// Written.
    Sent,
    /// Never written: the stream opened with an upstream error, which went
    /// out alone and ends it.
    Skipped,
}

/// What the first delta does to a held `message_start`.
pub(super) enum First<'a> {
    /// The upstream's input counts: `message_start` carries them.
    Usage(&'a Usage),
    /// An upstream error: it goes out alone, as Anthropic's own streams can
    /// open with one.
    Error,
    /// Anything else: `message_start` goes out with zeros, ahead of it.
    Other,
}

impl<'a> First<'a> {
    pub(super) fn of(d: &'a StreamDelta) -> Self {
        match d {
            StreamDelta::Usage(u) => Self::Usage(u),
            StreamDelta::Error(_) => Self::Error,
            _ => Self::Other,
        }
    }
}

/// `message_start` and the `ping` after it, with `usage`'s input counts
/// (as [`anthropic_usage`] writes them) or zeros.
pub(super) fn frames(id: &str, alias: &str, usage: Option<&Usage>) -> String {
    let usage = match usage {
        Some(u) => anthropic_usage(u, None),
        None => json!({"input_tokens": 0, "output_tokens": 0}),
    };
    let mut out = frame(
        Some("message_start"),
        &json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": alias,
                "content": Value::Array(vec![]),
                "stop_reason": null,
                "stop_sequence": null,
                "usage": usage,
            },
        })
        .to_string(),
    );
    out.push_str(&frame(Some("ping"), &json!({"type": "ping"}).to_string()));
    out
}

#[cfg(test)]
mod tests;
