//! A generic SSE record splitter and dialect-agnostic delta assembler
//! (api-docs design §6.3, §6.7). Every documented stream is fair game here —
//! OpenAI, Anthropic, `/v1/responses` — unlike `pages::chat_stream`'s one
//! fixed event set, which stays its own reader (§11: sharing one is out of
//! scope for now). Pure splitting; the fetch itself lives in `send.rs`.

use serde_json::Value;

/// One SSE record: `event` defaults to `"message"` per the spec; multiple
/// `data:` lines join with `\n`, matching a browser's own EventSource.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SseFrame {
    pub event: String,
    pub data: String,
}

fn parse_record(record: &str) -> Option<SseFrame> {
    let mut event = String::from("message");
    let mut data = String::new();
    let mut saw_data = false;
    let mut saw_field = false;
    for line in record.lines() {
        if line.is_empty() || line.starts_with(':') {
            // A keep-alive comment, or a blank line already inside the record.
            continue;
        }
        // `field: value` — one leading space after the colon is part of the
        // syntax, anything past it is the value's own (HTML SSE §9.2.6); a
        // line with no colon is a field with an empty value.
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        saw_field = true;
        match field {
            "event" => event = value.to_string(),
            "data" => {
                if saw_data {
                    data.push('\n');
                }
                data.push_str(value);
                saw_data = true;
            }
            _ => {} // id:/retry: — not used here.
        }
    }
    saw_field.then_some(SseFrame { event, data })
}

/// Buffers bytes across chunk boundaries and yields every complete record
/// (terminated by a blank line) found so far — the same contract as
/// `pages::chat_stream::find_record_end`, generalized to hold state across
/// calls, and to every line ending SSE allows: `\r\n` and a lone `\r` are
/// folded to `\n` on the way in (a `\r` that ends one chunk waits to see
/// whether the next one starts with its `\n`), since a relayed upstream
/// stream is not always lmgw's own `\n\n`.
#[derive(Default)]
pub struct SseSplitter {
    buf: Vec<u8>,
    /// The last byte pushed was a `\r` (already stored as `\n`): a `\n`
    /// arriving next is its other half, not a second line end.
    after_cr: bool,
}

impl SseSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseFrame> {
        for &b in bytes {
            match b {
                b'\n' if self.after_cr => self.after_cr = false,
                b'\r' => {
                    self.buf.push(b'\n');
                    self.after_cr = true;
                }
                _ => {
                    self.buf.push(b);
                    self.after_cr = false;
                }
            }
        }
        let mut out = Vec::new();
        while let Some(pos) = find_record_end(&self.buf) {
            let record: Vec<u8> = self.buf.drain(..pos + 2).collect();
            let text = String::from_utf8_lossy(&record);
            let text = text.trim_matches('\n');
            if text.is_empty() {
                continue;
            }
            if let Some(frame) = parse_record(text) {
                out.push(frame);
            }
        }
        out
    }
}

fn find_record_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

/// The streamed text so far, across every dialect a documented route answers
/// with (§6.7): OpenAI `choices[0].delta.content`, Anthropic
/// `content_block_delta`'s `delta.text`, and `/v1/responses`'
/// `response.output_text.delta`. Called on each new batch of frames, so the
/// caller can append rather than re-read them all. (The chat mini-API's bare
/// `delta` event is not read: that API is not documented, so the tester never
/// opens its stream — review R3 #7.)
pub fn assemble_text(frames: &[SseFrame]) -> String {
    let mut out = String::new();
    for f in frames {
        let Ok(v) = serde_json::from_str::<Value>(&f.data) else {
            continue;
        };
        if let Some(s) = v
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
        {
            out.push_str(s);
        } else if f.event == "content_block_delta" {
            if let Some(s) = v.pointer("/delta/text").and_then(Value::as_str) {
                out.push_str(s);
            }
        } else if f.event == "response.output_text.delta" {
            if let Some(s) = v.get("delta").and_then(Value::as_str) {
                out.push_str(s);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_split_across_chunks_still_parses() {
        let mut s = SseSplitter::new();
        assert!(s.push(b"event: delta\ndata: {\"tex").is_empty());
        let frames = s.push(b"t\":\"hi\"}\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "delta");
        assert_eq!(frames[0].data, "{\"text\":\"hi\"}");
    }

    #[test]
    fn keep_alive_comments_are_skipped() {
        let mut s = SseSplitter::new();
        let frames = s.push(b": keep-alive\n\nevent: message\ndata: {}\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "message");
    }

    #[test]
    fn multiple_data_lines_join_with_newlines() {
        let mut s = SseSplitter::new();
        let frames = s.push(b"data: line1\ndata: line2\n\n");
        assert_eq!(frames[0].data, "line1\nline2");
    }

    #[test]
    fn two_records_in_one_chunk_both_come_out() {
        let mut s = SseSplitter::new();
        let frames = s.push(b"data: a\n\ndata: b\n\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "a");
        assert_eq!(frames[1].data, "b");
    }

    #[test]
    fn crlf_records_split_even_when_the_pair_straddles_chunks() {
        let mut s = SseSplitter::new();
        assert!(s
            .push(b"event: delta\r\ndata: {\"text\":\"a\"}\r")
            .is_empty());
        let frames = s.push(b"\n\r\ndata: b\r\n\r\n");
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert_eq!(frames[0].event, "delta");
        assert_eq!(frames[0].data, "{\"text\":\"a\"}");
        assert_eq!(frames[1].data, "b");
    }

    #[test]
    fn lone_cr_line_ends_and_multi_line_data_with_crlf() {
        let mut s = SseSplitter::new();
        let frames = s.push(b"data: one\rdata: two\r\rdata:x\r\ndata:  y\r\n\r\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "one\ntwo");
        // Only the one space after the colon is syntax.
        assert_eq!(frames[1].data, "x\n y");
    }

    #[test]
    fn a_bare_data_field_and_an_empty_first_line_keep_their_lines() {
        let mut s = SseSplitter::new();
        let frames = s.push(b"data\ndata: b\n\n");
        assert_eq!(frames[0].data, "\nb");
    }

    #[test]
    fn assembled_text_reads_every_dialect_shape() {
        let frames = vec![
            SseFrame {
                event: "message".into(),
                data: r#"{"choices":[{"delta":{"content":"Hel"}}]}"#.into(),
            },
            SseFrame {
                event: "content_block_delta".into(),
                data: r#"{"delta":{"text":"lo"}}"#.into(),
            },
            SseFrame {
                event: "response.output_text.delta".into(),
                data: r#"{"delta":"?"}"#.into(),
            },
            // The undocumented chat mini-API's event is not a dialect here.
            SseFrame {
                event: "delta".into(),
                data: r#"{"text":"!"}"#.into(),
            },
        ];
        assert_eq!(assemble_text(&frames), "Hello?");
    }
}
