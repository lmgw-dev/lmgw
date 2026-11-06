//! Server-sent events read off a byte stream: the send stream (a POST, which
//! an `EventSource` cannot make) and the Chat's feed on a transport that is
//! not the browser's. Records end at a blank line; the terminator is ASCII,
//! so scanning bytes never splits a UTF-8 character inside a record.

/// One record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    /// `event:`, `message` when the record names none.
    pub event: String,
    /// The `data:` lines joined by `\n`.
    pub data: String,
    /// `id:`, when the record carries one: the cursor a reconnect resumes
    /// from (`Last-Event-ID`).
    pub id: Option<String>,
    /// `retry:`, the reconnect delay the server asks for (ms).
    pub retry: Option<u64>,
}

/// Bytes in, whole records out.
///
/// Each push scans only what it has not scanned before (`scanned`), and the
/// records it completes leave the buffer in one move: a frame of megabytes
/// arriving in small chunks (a read-aloud's audio as base64) and a catch-up
/// burst of thousands of records in one chunk both cost what their bytes
/// do, not their bytes times their chunks or records.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// No terminator starts before this offset of `buf`.
    scanned: usize,
    /// Every byte a scan looked at, for the test that keeps it linear.
    #[cfg(test)]
    looked_at: usize,
}

/// The longest record terminator (`\r\n\r\n`) less one: how far back a
/// terminator that the next chunk completes may start.
const OVERLAP: usize = 3;

impl Decoder {
    /// Take `chunk`; every record it completes, in order. A record of only
    /// comments (a keep-alive `:`) is none.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Record> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        // Where the record being read starts.
        let mut start = 0;
        loop {
            let from = self.scanned.max(start);
            let found = record_end(&self.buf, from);
            #[cfg(test)]
            {
                self.looked_at += found.map_or(self.buf.len(), |(end, len)| end + len) - from;
            }
            let Some((end, len)) = found else { break };
            if let Some(r) = parse(&String::from_utf8_lossy(&self.buf[start..end])) {
                out.push(r);
            }
            start = end + len;
            self.scanned = start;
        }
        // Nothing ends in the rest, except a terminator the next chunk
        // completes.
        let scanned = self.buf.len().saturating_sub(OVERLAP).max(start);
        self.buf.drain(..start);
        self.scanned = scanned - start;
        out
    }
}

/// Where the first record ending at or after `from` ends, and how long its
/// terminator is (`\n\n` or `\r\n\r\n`).
fn record_end(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..buf.len()).find_map(|i| match &buf[i..] {
        [b'\n', b'\n', ..] => Some((i, 2)),
        [b'\r', b'\n', b'\r', b'\n', ..] => Some((i, 4)),
        _ => None,
    })
}

fn parse(text: &str) -> Option<Record> {
    let mut r = Record {
        event: "message".into(),
        ..Record::default()
    };
    let mut fields = false;
    let mut data: Option<String> = None;
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        fields = true;
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        match name {
            "event" => r.event = value.trim().to_string(),
            "data" => {
                let d = data.get_or_insert_with(String::new);
                if !d.is_empty() {
                    d.push('\n');
                }
                d.push_str(value.trim_start());
            }
            // The spec ignores an id with a NUL in it.
            "id" if !value.contains('\0') => r.id = Some(value.trim_start().to_string()),
            "retry" => r.retry = value.trim().parse().ok(),
            _ => {}
        }
    }
    r.data = data.unwrap_or_default();
    fields.then_some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_split_across_chunks_come_out_whole() {
        let mut d = Decoder::default();
        assert!(d.push(b"event: delta\ndata: {\"te").is_empty());
        let got = d
            .push(b"xt\":\"h\xc3")
            .into_iter()
            .chain(d.push(b"\xa9\"}\n\n"));
        let got: Vec<Record> = got.collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].event, "delta");
        assert_eq!(got[0].data, "{\"text\":\"h\u{e9}\"}");
        assert_eq!(got[0].id, None);
    }

    #[test]
    fn ids_retry_comments_and_multi_line_data() {
        let mut d = Decoder::default();
        let got = d.push(
            b": keep-alive\n\nid: 41\nevent: thread.updated\ndata: a\ndata: b\n\nretry: 2500\n\ndata: plain\r\n\r\n",
        );
        assert_eq!(
            got,
            vec![
                Record {
                    event: "thread.updated".into(),
                    data: "a\nb".into(),
                    id: Some("41".into()),
                    retry: None,
                },
                Record {
                    event: "message".into(),
                    retry: Some(2500),
                    ..Record::default()
                },
                Record {
                    event: "message".into(),
                    data: "plain".into(),
                    ..Record::default()
                },
            ]
        );
    }

    #[test]
    fn an_event_without_data_is_still_a_record() {
        // The send stream's `stop` carries none.
        let mut d = Decoder::default();
        let got = d.push(b"event: stop\n\n");
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].event.as_str(), got[0].data.as_str()), ("stop", ""));
    }

    #[test]
    fn a_large_frame_in_small_chunks_and_a_burst_cost_their_bytes() {
        // A read-aloud's audio: one record of 3 MiB of base64, in 256-byte
        // chunks, a terminator split across two of them.
        let payload = "QUJD".repeat(3 * 1024 * 1024 / 4);
        let frame = format!("id: 9\nevent: speech\ndata: {{\"audio\":\"{payload}\"}}\r\n\r\n");
        let mut d = Decoder::default();
        let mut got = Vec::new();
        for c in frame.as_bytes().chunks(256) {
            got.extend(d.push(c));
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id.as_deref(), Some("9"));
        assert_eq!(got[0].data.len(), payload.len() + 12);
        let pushes = frame.len().div_ceil(256);
        assert!(
            d.looked_at <= frame.len() + OVERLAP * pushes,
            "scanned {} bytes for {}",
            d.looked_at,
            frame.len()
        );

        // A catch-up: 20 000 records in one chunk, in order.
        let burst: String = (0..20_000)
            .map(|i| format!("id: e:{i}\nevent: thread.updated\ndata: {{\"thread_id\":{i}}}\n\n"))
            .collect();
        let mut d = Decoder::default();
        let got = d.push(burst.as_bytes());
        assert_eq!(got.len(), 20_000);
        assert!(got
            .iter()
            .enumerate()
            .all(|(i, r)| r.id.as_deref() == Some(format!("e:{i}").as_str())));
        assert!(
            d.looked_at <= burst.len(),
            "{} for {}",
            d.looked_at,
            burst.len()
        );
        assert!(d.buf.is_empty());
    }

    #[test]
    fn a_terminator_split_at_every_point_still_ends_its_record() {
        let text = b"data: a\r\n\r\ndata: b\n\ndata: c\r\n\r\n";
        for cut in 0..text.len() {
            let mut d = Decoder::default();
            let mut got = d.push(&text[..cut]);
            got.extend(d.push(&text[cut..]));
            let data: Vec<&str> = got.iter().map(|r| r.data.as_str()).collect();
            assert_eq!(data, ["a", "b", "c"], "cut at {cut}");
        }
    }
}
