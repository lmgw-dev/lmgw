//! Minimal incremental SSE parser for upstream byte streams.
//!
//! Deliberately not `reqwest-eventsource`: that crate auto-reconnects by
//! re-sending the request, which must never happen for a proxied chat POST.

/// One parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental decoder: feed raw bytes, get complete events.
///
/// Each byte is decoded, normalized and searched once: a feed works on what
/// it added, never on the whole buffer again, so a multi-megabyte event (an
/// `image_generation` partial image) split into many small chunks costs what
/// its length costs, not its length times its chunk count.
///
/// The search resumes one byte before the end of what the last feed left,
/// and that byte may be inside a multi-byte char (`data: €` then `\n\n`):
/// the search runs over bytes, and the boundary it finds is ASCII, so the
/// split points it hands out are always char boundaries.
///
/// A trailing `\r` is held until the next byte says whether it is half of a
/// CRLF. A server that ends its lines in lone CRs therefore has each event
/// handed out one chunk late, when the next chunk arrives; [`finish`]
/// flushes the last one at the end of the stream.
///
/// [`finish`]: SseDecoder::finish
#[derive(Default)]
pub struct SseDecoder {
    /// Decoded, CRLF-normalized text not yet handed out as events.
    buf: String,
    /// The bytes of an incomplete UTF-8 sequence that ended the last chunk.
    pending: Vec<u8>,
    /// The last chunk ended in `\r`: a `\n` opening the next one is the
    /// same line break, not a second one.
    cr: bool,
    /// Up to where `buf` holds no event boundary: the search resumes here.
    /// One byte short of the end, so a `\n\n` split across feeds is seen;
    /// a byte index, not necessarily a char boundary.
    scanned: usize,
    /// Bytes searched for a boundary so far: what the tests bound.
    #[cfg(test)]
    searched: usize,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes; returns all events completed by this chunk.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        // Only complete UTF-8 prefixes are decoded. A sequence that can never
        // become valid is one U+FFFD and decoding goes on past it, as a lossy
        // decode would: kept pending, it would hold back every byte after it
        // for the rest of the stream. Only the held-over bytes and this
        // chunk's are decoded.
        let held;
        let bytes: &[u8] = if self.pending.is_empty() {
            chunk
        } else {
            let mut b = std::mem::take(&mut self.pending);
            b.extend_from_slice(chunk);
            held = b;
            &held
        };
        let mut text = String::with_capacity(bytes.len());
        let mut rest = bytes;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    text.push_str(s);
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    text.push_str(std::str::from_utf8(&rest[..valid]).unwrap());
                    match e.error_len() {
                        Some(bad) => {
                            text.push(char::REPLACEMENT_CHARACTER);
                            rest = &rest[valid + bad..];
                        }
                        // An incomplete sequence at the end: the next chunk
                        // may finish it.
                        None => {
                            self.pending = rest[valid..].to_vec();
                            break;
                        }
                    }
                }
            }
        }
        self.push_normalized(&text);
        self.drain_events()
    }

    /// The end of the stream: a `\r` held back to see whether a `\n`
    /// follows is a line break after all, and the events it completes are
    /// handed out. An event the stream cut off before its blank line is
    /// dropped, as the SSE spec says.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        if !std::mem::take(&mut self.cr) {
            return Vec::new();
        }
        self.buf.push('\n');
        self.drain_events()
    }

    fn drain_events(&mut self) -> Vec<SseEvent> {
        // Every boundary in what this feed added, from the byte before it.
        let mut events = Vec::new();
        let mut start = 0;
        let mut from = self.scanned;
        loop {
            // Bytes, not `str`: `from` may sit inside a multi-byte char.
            let found = memchr::memmem::find(&self.buf.as_bytes()[from..], b"\n\n");
            #[cfg(test)]
            {
                self.searched += found.map_or(self.buf.len() - from, |rel| rel + 2);
            }
            let Some(rel) = found else { break };
            let end = from + rel + 2;
            if let Some(ev) = parse_event(&self.buf[start..end]) {
                events.push(ev);
            }
            start = end;
            from = end;
        }
        // One drain per feed, not one per event: each would shift the rest.
        if start > 0 {
            self.buf.drain(..start);
        }
        // A `\n` at the very end may be the first half of a boundary.
        self.scanned = self.buf.len().saturating_sub(1);
        events
    }

    /// Append `text` with each CRLF and lone CR as `\n`, so event boundaries
    /// are plain `\n\n`. A trailing `\r` waits for the next byte to say
    /// whether it is half of a CRLF.
    fn push_normalized(&mut self, text: &str) {
        let mut chars = text.chars().peekable();
        if self.cr && !text.is_empty() {
            self.cr = false;
            self.buf.push('\n');
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
        }
        if !text.contains('\r') {
            self.buf.extend(chars);
            return;
        }
        while let Some(c) = chars.next() {
            if c != '\r' {
                self.buf.push(c);
                continue;
            }
            match chars.peek() {
                Some('\n') => {
                    chars.next();
                    self.buf.push('\n');
                }
                Some(_) => self.buf.push('\n'),
                None => self.cr = true,
            }
        }
    }
}

fn parse_event(raw: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim_start_matches(' ').to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // id:, retry:, comments (:) ignored
    }
    if event.is_none() && data_lines.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data_lines.join("\n"),
    })
}

/// Format one outgoing SSE frame.
pub fn frame(event: Option<&str>, data: &str) -> String {
    match event {
        Some(ev) => format!("event: {ev}\ndata: {data}\n\n"),
        None => format!("data: {data}\n\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_chunks() {
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: {\"a\":").is_empty());
        let evs = d.feed(b"1}\n\ndata: [DONE]\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].data, "{\"a\":1}");
        assert_eq!(evs[1].data, "[DONE]");
    }

    #[test]
    fn parses_named_events_and_crlf() {
        let mut d = SseDecoder::new();
        let evs = d.feed(b"event: message_start\r\ndata: {}\r\n\r\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("message_start"));
        assert_eq!(evs[0].data, "{}");
    }

    #[test]
    fn invalid_utf8_is_replaced_and_decoding_goes_on() {
        let mut d = SseDecoder::new();
        // 0xff can start no sequence; 0xe2 0x82 is cut short by a space.
        let evs = d.feed(b"data: a\xffb\xe2\x82 c\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "a\u{FFFD}b\u{FFFD} c");
        let evs = d.feed(b"data: next\n\n");
        assert_eq!(evs.len(), 1, "the stream reads on after the bad bytes");
        assert_eq!(evs[0].data, "next");
    }

    #[test]
    fn a_sequence_split_across_chunks_is_kept_whole() {
        let mut d = SseDecoder::new();
        let euro = "\u{20AC}".as_bytes();
        assert!(d.feed(b"data: ").is_empty());
        assert!(d.feed(&euro[..2]).is_empty());
        let mut rest = euro[2..].to_vec();
        rest.extend_from_slice(b"\n\n");
        let evs = d.feed(&rest);
        assert_eq!(evs[0].data, "\u{20AC}");
    }

    #[test]
    fn multiline_data() {
        let mut d = SseDecoder::new();
        let evs = d.feed(b"data: a\ndata: b\n\n");
        assert_eq!(evs[0].data, "a\nb");
    }

    #[test]
    fn a_crlf_split_across_chunks_is_one_line_break() {
        let mut d = SseDecoder::new();
        assert!(d.feed(b"event: a\r").is_empty());
        assert!(d.feed(b"\ndata: 1\r").is_empty(), "no boundary yet");
        let evs = d.feed(b"\n\r\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(
            (evs[0].event.as_deref(), evs[0].data.as_str()),
            (Some("a"), "1")
        );
        // A lone CR is a line break of its own.
        let evs = d.feed(b"data: 2\r\rdata: 3\n\n");
        assert_eq!(
            evs.iter().map(|e| e.data.as_str()).collect::<Vec<_>>(),
            ["2", "3"]
        );
    }

    /// A multi-megabyte event (an `image_generation` partial image) in many
    /// small chunks: every byte is searched about once, where a rescan of
    /// the whole buffer per feed would search it once per chunk after it.
    #[test]
    fn a_large_event_in_small_chunks_is_searched_once() {
        let payload = "A".repeat(4 << 20);
        let wire = format!(
            "event: response.image_generation_call.partial_image\r\ndata: {{\"partial_image_b64\":\"{payload}\"}}\r\n\r\ndata: after\n\n"
        );
        let mut d = SseDecoder::new();
        let mut events = Vec::new();
        for chunk in wire.as_bytes().chunks(61) {
            events.extend(d.feed(chunk));
        }
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data.len(), payload.len() + 24);
        assert_eq!(events[1].data, "after");
        let chunks = wire.len().div_ceil(61);
        assert!(
            d.searched <= wire.len() + 2 * chunks,
            "searched {} bytes for {} bytes in {chunks} chunks",
            d.searched,
            wire.len()
        );
    }

    #[test]
    fn several_events_in_one_chunk_and_a_boundary_split_across_two() {
        let mut d = SseDecoder::new();
        let evs = d.feed(b"data: 1\n\ndata: 2\n\ndata: 3\n");
        assert_eq!(evs.len(), 2);
        let evs = d.feed(b"\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "3");
    }

    /// A chunk ending in a multi-byte char leaves the resume point inside
    /// it; the next feed's search must not slice there.
    #[test]
    fn a_chunk_ending_in_a_multibyte_char_does_not_split_it() {
        let mut d = SseDecoder::new();
        assert!(d.feed("data: \u{20AC}".as_bytes()).is_empty());
        let evs = d.feed(b"\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "\u{20AC}");

        // The same after a drain: the leftover ends in the char.
        let mut d = SseDecoder::new();
        let evs = d.feed("data: 1\n\ndata: \u{1F600}".as_bytes());
        assert_eq!(evs.len(), 1);
        let evs = d.feed(b"\n\n");
        assert_eq!(evs[0].data, "\u{1F600}");
    }

    /// Every split position of a wire full of multi-byte chars (é, €, an
    /// emoji, next to the boundaries) gives what the whole wire gives.
    #[test]
    fn every_split_of_a_multibyte_wire_matches_the_whole() {
        let wire =
            "data: \u{e9}\n\nevent: \u{20AC}\r\ndata: \u{1F600}\u{e9}\r\n\r\ndata: x\u{20AC}\n\n"
                .as_bytes();
        let whole = decode(&[wire]);
        assert_eq!(whole.len(), 3);
        for i in 0..=wire.len() {
            for j in i..=wire.len() {
                let split = decode(&[&wire[..i], &wire[i..j], &wire[j..]]);
                assert_eq!(split, whole, "split at {i}, {j}");
            }
        }
    }

    #[test]
    fn finish_flushes_a_held_cr() {
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: 1\r\r").is_empty(), "the last CR is held");
        let evs = d.finish();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "1");
        assert!(d.finish().is_empty());
    }

    /// Seeded random wires built from the bytes that matter (line breaks of
    /// every kind, field names, multi-byte chars, a bad byte) fed at random
    /// split points give what the same wire fed whole gives.
    #[test]
    fn random_splits_match_the_whole_wire() {
        const PIECES: &[&[u8]] = &[
            b"data: ",
            b"event: ",
            b"id: 7",
            b": c",
            b"\n",
            b"\r",
            b"\r\n",
            b"\n\n",
            b"a",
            b"{}",
            "\u{e9}".as_bytes(),
            "\u{20AC}".as_bytes(),
            "\u{1F600}".as_bytes(),
            b"\xff",
            b"\xe2\x82",
        ];
        let mut rng = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = |n: usize| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % n as u64) as usize
        };
        for case in 0..20_000 {
            let mut wire = Vec::new();
            for _ in 0..next(24) {
                wire.extend_from_slice(PIECES[next(PIECES.len())]);
            }
            let whole = decode(&[&wire]);
            let mut chunks = Vec::new();
            let mut at = 0;
            while at < wire.len() {
                let len = 1 + next(wire.len() - at);
                chunks.push(&wire[at..at + len]);
                at += len;
            }
            assert_eq!(
                decode(&chunks),
                whole,
                "case {case}: {wire:?} as {chunks:?}"
            );
        }
    }

    fn decode(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut d = SseDecoder::new();
        let mut evs: Vec<SseEvent> = chunks.iter().flat_map(|c| d.feed(c)).collect();
        evs.extend(d.finish());
        evs
    }
}
