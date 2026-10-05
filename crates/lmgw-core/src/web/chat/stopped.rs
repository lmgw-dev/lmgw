//! What a plain chat turn's relay read, kept for its row when the turn is
//! stopped (WP11 server review M2).
//!
//! A plain turn races its stream against the turn's stop
//! (`Turn::or_stop`): a stop drops the stream's future where it stands, so
//! the turn ends at once and its `done` goes out without waiting for the
//! upstream's next chunk — a bound session's journal, its next response's
//! barrier and its drain all wait for that `done`. The numbers the dropped
//! future had gathered would go with it, and the row would say the stopped
//! call cost nothing: every barge-in in voice mode, every Stop, would take
//! its answer budget-free. So the relay notes here, outside that future,
//! what it read, and a stopped turn's row says what the call cost so far
//! as the stock realtime path's does (`proxy::stopped_usage`): the
//! upstream's usage or llama.cpp's counters where it sent them, and
//! otherwise ~4 characters a token, which the row's message names. A stop
//! after the request went out and before the upstream answered counts the
//! prompt alone (`proxy::unanswered_usage`).
//!
//! The reply's own token counts and the `done` frame keep what the upstream
//! reported; only the row carries the estimate, with its note.

use std::time::Instant;

use crate::ir::{ChatRequest, StreamDelta, Timings, Usage};
use crate::proxy::{self, StreamOutcome};

/// What the relay has read of the stream so far.
#[derive(Default)]
pub(super) struct Read {
    usage: Usage,
    timings: Option<Timings>,
    ttfb_ms: Option<i64>,
    /// Characters of output (text, reasoning, tool calls) delivered.
    pub produced: usize,
}

impl Read {
    /// Note one delta, as `drive_upstream` does in its own outcome.
    pub fn note(&mut self, d: &StreamDelta, started: Instant) {
        self.produced += proxy::produced_chars(d);
        match d {
            StreamDelta::TextDelta(_) | StreamDelta::ToolCallStart { .. } => {
                self.ttfb_ms
                    .get_or_insert_with(|| started.elapsed().as_millis() as i64);
            }
            StreamDelta::Usage(u) => self.usage.merge(u),
            StreamDelta::Timings(t) => self.timings = Some(*t),
            _ => {}
        }
    }

    /// The outcome of a stream the turn's stop dropped.
    pub fn stopped(&self) -> StreamOutcome {
        StreamOutcome {
            ttfb_ms: self.ttfb_ms,
            usage: self.usage,
            timings: self.timings,
            aborted: true,
            ..Default::default()
        }
    }
}

/// The usage and the `canceled` note of a stream that ended early without
/// an error — the turn's stop, or a reader that went away — or `None` for
/// one that ran to its end or failed.
pub(super) fn usage(
    outcome: &StreamOutcome,
    ir: &ChatRequest,
    produced: usize,
) -> Option<(Usage, String)> {
    (outcome.aborted && outcome.error.is_none())
        .then(|| proxy::stopped_usage(outcome.usage, outcome.timings.as_ref(), ir, produced))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ChatRequest {
        ChatRequest {
            model_alias: "m".into(),
            messages: vec![crate::ir::Message::text(crate::ir::Role::User, "abcdefgh")],
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
    fn a_dropped_stream_keeps_what_was_read() {
        let mut read = Read::default();
        let started = Instant::now();
        read.note(&StreamDelta::ReasoningDelta("hmm".into()), started);
        read.note(&StreamDelta::TextDelta("Hallo".into()), started);
        read.note(
            &StreamDelta::Usage(Usage {
                prompt_tokens: Some(40),
                ..Default::default()
            }),
            started,
        );
        assert_eq!(read.produced, 8);
        let outcome = read.stopped();
        assert!(outcome.aborted && outcome.ttfb_ms.is_some());
        let (u, note) = usage(&outcome, &request(), read.produced).unwrap();
        // The upstream's prompt count stays; the output is estimated.
        assert_eq!((u.prompt_tokens, u.completion_tokens), (Some(40), Some(2)));
        assert!(note.contains("completion tokens estimated"), "{note}");
    }

    #[test]
    fn a_whole_or_failed_stream_is_no_stop() {
        let whole = StreamOutcome {
            completed: true,
            ..Default::default()
        };
        assert!(usage(&whole, &request(), 3).is_none());
        let failed = StreamOutcome {
            aborted: true,
            error: Some(("upstream".into(), "boom".into())),
            ..Default::default()
        };
        assert!(usage(&failed, &request(), 3).is_none());
    }
}
