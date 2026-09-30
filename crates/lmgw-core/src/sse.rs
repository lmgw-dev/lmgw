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
#[derive(Default)]
pub struct SseDecoder {
    buf: String,
    pending: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes; returns all events completed by this chunk.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        // Accumulate bytes, only decode complete UTF-8 prefixes.
        self.pending.extend_from_slice(chunk);
        match std::str::from_utf8(&self.pending) {
            Ok(s) => {
                self.buf.push_str(s);
                self.pending.clear();
            }
            Err(e) => {
                let valid = e.valid_up_to();
                let s = std::str::from_utf8(&self.pending[..valid]).unwrap();
                self.buf.push_str(s);
                self.pending.drain(..valid);
                // keep incomplete trailing bytes pending
            }
        }

        let mut events = Vec::new();
        // Normalize CRLF once so event boundaries are plain "\n\n".
        if self.buf.contains('\r') {
            self.buf = self.buf.replace("\r\n", "\n").replace('\r', "\n");
        }
        while let Some(pos) = self.buf.find("\n\n") {
            let raw: String = self.buf.drain(..pos + 2).collect();
            if let Some(ev) = parse_event(&raw) {
                events.push(ev);
            }
        }
        events
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
    fn multiline_data() {
        let mut d = SseDecoder::new();
        let evs = d.feed(b"data: a\ndata: b\n\n");
        assert_eq!(evs[0].data, "a\nb");
    }
}
