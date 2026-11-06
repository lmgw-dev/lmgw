//! A streamed tool call that ends without argument text is a call with no
//! arguments, `{}` ([`crate::ir::parse_call_args`]), and every client wire
//! hears it so: OpenAI streams `"arguments": "{}"` for one, while Anthropic
//! streams no `input_json_delta` at all and an OpenAI-compatible upstream may
//! send nothing past the call's name. [`NoArgsCalls`] wraps a stream's
//! decoder and, ahead of the stream's stop, sends one
//! [`StreamDelta::ToolCallArgsDelta`] of `{}` for every call that got no
//! fragment that was not blank. A call whose fragments carried text keeps
//! exactly what the upstream sent.
//!
//! One place for every stream (`proxy::drive_upstream_until`), so the chat,
//! messages and responses wires, the Chat and the tool loop read the same
//! deltas: before it, `/v1/chat/completions` streamed such a call with
//! arguments `""`, and a client replaying it to an Anthropic upstream was
//! refused ("tool_use.input: Input should be an object").

use std::collections::BTreeMap;

use super::EgressStreamDecoder;
use crate::ir::StreamDelta;
use crate::sse::SseEvent;

/// The argument text of a call with no arguments.
pub const NO_ARGS: &str = "{}";

/// [`EgressStreamDecoder`] that closes argument-less calls (module doc).
pub struct NoArgsCalls {
    inner: Box<dyn EgressStreamDecoder>,
    /// Each started call's index, and whether a non-blank fragment came.
    calls: BTreeMap<usize, bool>,
}

impl NoArgsCalls {
    pub fn wrap(inner: Box<dyn EgressStreamDecoder>) -> Box<dyn EgressStreamDecoder> {
        Box::new(Self {
            inner,
            calls: BTreeMap::new(),
        })
    }

    /// `batch` with the `{}` deltas ahead of its stop, once per call.
    fn close(&mut self, batch: Vec<StreamDelta>) -> Vec<StreamDelta> {
        let mut out = Vec::with_capacity(batch.len());
        for d in batch {
            match &d {
                StreamDelta::ToolCallStart { index, .. } => {
                    self.calls.entry(*index).or_insert(false);
                }
                StreamDelta::ToolCallArgsDelta { index, fragment } => {
                    if !fragment.trim().is_empty() {
                        self.calls.insert(*index, true);
                    }
                }
                StreamDelta::Stop(_) => {
                    for (index, seen) in self.calls.iter_mut().filter(|(_, s)| !**s) {
                        *seen = true;
                        out.push(StreamDelta::ToolCallArgsDelta {
                            index: *index,
                            fragment: NO_ARGS.into(),
                        });
                    }
                }
                _ => {}
            }
            out.push(d);
        }
        out
    }
}

impl EgressStreamDecoder for NoArgsCalls {
    fn on_event(&mut self, ev: &SseEvent) -> Vec<StreamDelta> {
        let batch = self.inner.on_event(ev);
        self.close(batch)
    }

    fn on_end(&mut self) -> Vec<StreamDelta> {
        let batch = self.inner.on_end();
        self.close(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::FinishReason;

    struct Fixed(Vec<Vec<StreamDelta>>);

    impl EgressStreamDecoder for Fixed {
        fn on_event(&mut self, _: &SseEvent) -> Vec<StreamDelta> {
            if self.0.is_empty() {
                Vec::new()
            } else {
                self.0.remove(0)
            }
        }
    }

    fn start(index: usize) -> StreamDelta {
        StreamDelta::ToolCallStart {
            index,
            id: format!("c{index}"),
            name: "now".into(),
        }
    }

    fn args(index: usize, fragment: &str) -> StreamDelta {
        StreamDelta::ToolCallArgsDelta {
            index,
            fragment: fragment.into(),
        }
    }

    fn run(batches: Vec<Vec<StreamDelta>>) -> Vec<StreamDelta> {
        let n = batches.len();
        let mut dec = NoArgsCalls::wrap(Box::new(Fixed(batches)));
        let ev = SseEvent {
            event: None,
            data: String::new(),
        };
        (0..n).flat_map(|_| dec.on_event(&ev)).collect()
    }

    #[test]
    fn a_call_without_fragments_gets_empty_object_arguments_before_the_stop() {
        let stop = StreamDelta::Stop(FinishReason::ToolUse);
        let out = run(vec![
            vec![start(0)],
            vec![start(1)],
            vec![args(1, "{\"city\":"), args(1, "\"Paris\"}")],
            vec![args(0, " "), stop.clone()],
        ]);
        assert_eq!(
            out,
            vec![
                start(0),
                start(1),
                args(1, "{\"city\":"),
                args(1, "\"Paris\"}"),
                args(0, " "),
                args(0, "{}"),
                stop,
            ]
        );
    }

    #[test]
    fn a_stream_without_calls_or_without_a_stop_is_untouched() {
        let stop = StreamDelta::Stop(FinishReason::Stop);
        let text = StreamDelta::TextDelta("hi".into());
        assert_eq!(
            run(vec![vec![text.clone(), stop.clone()]]),
            vec![text, stop]
        );
        // Cut before its stop: nothing is invented for a call that may
        // still have been streaming.
        assert_eq!(run(vec![vec![start(0)]]), vec![start(0)]);
    }
}
