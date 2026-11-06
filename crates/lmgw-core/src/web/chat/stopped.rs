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

use crate::ir::{StreamDelta, Timings, Usage};
use crate::proxy::{self, StreamOutcome};

/// What the relay has read of the stream so far.
#[derive(Default)]
pub(super) struct Read {
    usage: Usage,
    timings: Option<Timings>,
    ttfb_ms: Option<i64>,
    /// The last `service_tier` the stream reported.
    service_tier: Option<String>,
    /// Characters of output (text, reasoning, tool calls) delivered.
    pub produced: usize,
    /// The output text delivered, as a platform row without usage is
    /// counted by (text and tool arguments, no reasoning; openai egress
    /// design §13.4). Kept only when counting.
    relayed: Option<String>,
}

impl Read {
    /// A reader that keeps the output text as well when `count_text` is set
    /// (an `openai` row).
    pub fn counting(count_text: bool) -> Self {
        Self {
            relayed: count_text.then(String::new),
            ..Self::default()
        }
    }

    /// The output text kept ([`Self::counting`]), empty when none was.
    pub fn take_relayed(&mut self) -> String {
        self.relayed.take().unwrap_or_default()
    }

    /// Note one delta, as `drive_upstream` does in its own outcome.
    pub fn note(&mut self, d: &StreamDelta, started: Instant) {
        self.produced += proxy::produced_chars(d);
        if let Some(text) = self.relayed.as_mut() {
            match d {
                StreamDelta::TextDelta(t) | StreamDelta::RefusalDelta(t) => text.push_str(t),
                StreamDelta::ToolCallArgsDelta { fragment, .. } => text.push_str(fragment),
                _ => {}
            }
        }
        match d {
            StreamDelta::TextDelta(_)
            | StreamDelta::RefusalDelta(_)
            | StreamDelta::ToolCallStart { .. } => {
                self.ttfb_ms
                    .get_or_insert_with(|| started.elapsed().as_millis() as i64);
            }
            StreamDelta::Usage(u) => self.usage.merge(u),
            StreamDelta::Timings(t) => self.timings = Some(*t),
            StreamDelta::ServiceTier(t) => self.service_tier = Some(t.clone()),
            _ => {}
        }
    }

    /// The outcome of a stream the turn's stop dropped.
    pub fn stopped(&self) -> StreamOutcome {
        StreamOutcome {
            ttfb_ms: self.ttfb_ms,
            usage: self.usage,
            timings: self.timings,
            service_tier: self.service_tier.clone(),
            aborted: true,
            ..Default::default()
        }
    }
}

/// Whether a stream ended early without an error — the turn's stop, or a
/// reader that went away — rather than running to its end or failing: its
/// row is `canceled`, with what it cost so far (`proxy::stopped_cost`).
pub(super) fn is_stop(outcome: &StreamOutcome) -> bool {
    outcome.aborted && outcome.error.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::ChatRequest;

    fn request() -> ChatRequest {
        ChatRequest {
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
        assert!(is_stop(&outcome));
        let (u, note) = proxy::stopped_usage(
            outcome.usage,
            outcome.timings.as_ref(),
            &request(),
            read.produced,
            None,
        );
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
        assert!(!is_stop(&whole));
        let failed = StreamOutcome {
            aborted: true,
            error: Some(("upstream".into(), "boom".into())),
            ..Default::default()
        };
        assert!(!is_stop(&failed));
    }

    #[test]
    fn only_a_counting_reader_keeps_the_output_text() {
        let started = Instant::now();
        let deltas = [
            StreamDelta::ReasoningDelta("hmm".into()),
            StreamDelta::TextDelta("Hallo".into()),
            StreamDelta::ToolCallArgsDelta {
                index: 0,
                fragment: "{}".into(),
            },
        ];
        let mut counting = Read::counting(true);
        let mut plain = Read::counting(false);
        for d in &deltas {
            counting.note(d, started);
            plain.note(d, started);
        }
        assert_eq!(counting.take_relayed(), "Hallo{}");
        assert_eq!(plain.take_relayed(), "");
    }
}
