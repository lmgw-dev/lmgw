//! The Chat change feed as a client reads it (`GET /chat/api/feed`;
//! client-apps design §2): an incremental `text/event-stream` decoder
//! ([`SseDecoder`]) and a reader that types the events and keeps the cursor
//! to resume from ([`FeedReader`]).
//!
//! Bytes go in as they arrive, in chunks of any size; events come out. The
//! cursor (`"<epoch>:<seq>:<tag>"`, opaque text a client hands back as it
//! got it) is every stored event's `id:`, the keep-alive
//! comment's `id:` (a stretch of events the reader may not see moves it),
//! and `hello.cursor`, where the stream continues. A client resumes with it
//! as `Last-Event-ID` ([`crate::requests::feed`]). A `resync` means the
//! cursor could not be honoured: reload what is shown.
//!
//! A dead link is the client's to time: `hello.keepalive_s` says how often
//! a keep-alive comes ([`FeedReader::keepalive_s`]), and every byte that
//! arrives is a sign of life.

pub use lmgw_api_types::chat_feed::{
    Cursor, FeedEvent, FeedEventError, FolderNow, Hello, ThreadNow, EVENTS,
};

/// One SSE record: its `event:` (`message` when it names none), its
/// `data:` lines joined with `\n`, and the `id:` it carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub id: Option<String>,
    pub event: String,
    pub data: String,
}

/// What the decoder makes of the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseItem {
    /// A record with data.
    Frame(SseFrame),
    /// A record with an `id:` and no data: only the last event id moved.
    Id(String),
    /// A comment line (`: keep-alive`).
    Comment(String),
}

/// An incremental `text/event-stream` decoder, after the HTML standard's
/// rules: lines end with LF, CRLF or CR; a leading byte-order mark is
/// dropped; `:` starts a comment; a field's value loses one leading space;
/// a blank line ends a record.
#[derive(Debug, Clone, Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    /// The last byte was a CR: an LF next ends no second line.
    after_cr: bool,
    /// A line was completed: a byte-order mark can no longer come.
    past_first: bool,
    event: String,
    data: String,
    has_data: bool,
    id: Option<String>,
    last_event_id: String,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The last `id:` the stream set (empty before any).
    pub fn last_event_id(&self) -> &str {
        &self.last_event_id
    }

    /// Feed `bytes`; the records and comments they complete, in order.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseItem> {
        let mut out = Vec::new();
        for &b in bytes {
            match b {
                b'\n' if self.after_cr => self.after_cr = false,
                b'\n' | b'\r' => {
                    self.after_cr = b == b'\r';
                    let line = std::mem::take(&mut self.line);
                    let mut line = line.as_slice();
                    if !std::mem::replace(&mut self.past_first, true) {
                        line = line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line);
                    }
                    self.line_done(&String::from_utf8_lossy(line), &mut out);
                }
                _ => {
                    self.after_cr = false;
                    self.line.push(b);
                }
            }
        }
        out
    }

    fn line_done(&mut self, line: &str, out: &mut Vec<SseItem>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if let Some(comment) = line.strip_prefix(':') {
            out.push(SseItem::Comment(
                comment.strip_prefix(' ').unwrap_or(comment).to_string(),
            ));
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "id" if !value.contains('\0') => {
                self.id = Some(value.to_string());
                self.last_event_id = value.to_string();
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseItem>) {
        let id = self.id.take();
        let event = std::mem::take(&mut self.event);
        if !std::mem::take(&mut self.has_data) {
            if let Some(id) = id {
                out.push(SseItem::Id(id));
            }
            return;
        }
        out.push(SseItem::Frame(SseFrame {
            id,
            event: if event.is_empty() {
                "message".into()
            } else {
                event
            },
            data: std::mem::take(&mut self.data),
        }));
    }
}

/// What the reader makes of the feed.
///
/// Nothing stops a reader: an event of a type it does not know is
/// [`FeedItem::Unknown`], one it knows whose data does not read is
/// [`FeedItem::Unreadable`], and the stream goes on.
#[derive(Debug, Clone, PartialEq)]
// Owned values, no boxes: an FFI wrapper maps each variant as it is.
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
pub enum FeedItem {
    /// An event, typed. Never [`FeedEvent::Unknown`]: that is
    /// [`FeedItem::Unknown`].
    Event(FeedEvent),
    /// A keep-alive: the link is alive (the cursor may have moved).
    KeepAlive,
    /// An event of a type this build does not know, as it came: one a
    /// newer gateway added. The cursor has moved past it, as past any
    /// event, so a resumed stream does not send it again: skip it and log
    /// it.
    Unknown { event: String, data: String },
    /// An event of a type this build knows whose data does not read
    /// ([`FeedEvent::parse`] says why), as it came. The cursor has moved past
    /// it, so what it was about — a thread, a folder, what is live — may now
    /// be shown stale: reload what you show, as for `resync`.
    Unreadable { event: String, data: String },
}

/// The feed of one connection: typed events, and the cursor to resume
/// from after it ends.
#[derive(Debug, Clone, Default)]
pub struct FeedReader {
    sse: SseDecoder,
    cursor: Option<String>,
    keepalive_s: Option<u32>,
}

impl FeedReader {
    /// A reader for a connection opened with `resume` (`None`: from now).
    pub fn new(resume: Option<&str>) -> Self {
        FeedReader {
            cursor: resume.filter(|c| !c.is_empty()).map(str::to_string),
            ..Default::default()
        }
    }

    /// Feed `bytes`; the events they complete, in order.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<FeedItem> {
        let mut out = Vec::new();
        for item in self.sse.push(bytes) {
            match item {
                SseItem::Comment(_) => out.push(FeedItem::KeepAlive),
                SseItem::Id(id) => {
                    self.moved(id);
                    out.push(FeedItem::KeepAlive);
                }
                SseItem::Frame(f) => {
                    if let Some(id) = f.id {
                        self.moved(id);
                    }
                    match FeedEvent::parse(&f.event, &f.data) {
                        Ok(FeedEvent::Unknown { event, data }) => {
                            out.push(FeedItem::Unknown { event, data })
                        }
                        Ok(ev) => {
                            if let FeedEvent::Hello(h) = &ev {
                                self.keepalive_s = Some(h.keepalive_s);
                                self.moved(h.cursor.clone());
                            }
                            out.push(FeedItem::Event(ev));
                        }
                        Err(_) => out.push(FeedItem::Unreadable {
                            event: f.event,
                            data: f.data,
                        }),
                    }
                }
            }
        }
        out
    }

    fn moved(&mut self, cursor: String) {
        if !cursor.is_empty() {
            self.cursor = Some(cursor);
        }
    }

    /// The cursor to resume from: `Last-Event-ID` of the next connection.
    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    /// `hello.keepalive_s`, once `hello` came.
    pub fn keepalive_s(&self) -> Option<u32> {
        self.keepalive_s
    }

    /// The request that resumes this feed.
    pub fn resume(&self) -> crate::requests::Request {
        crate::requests::feed(self.cursor())
    }
}

#[cfg(test)]
mod tests {
    use lmgw_api_types::chat_feed::{TurnStarted, VoiceEnded};

    use super::*;

    #[test]
    fn records_split_anywhere_decode_alike() {
        let stream =
            b"\xEF\xBB\xBFevent: hello\r\ndata: {\"a\":1}\r\n\r\n: keep-alive\nid: e:5\n\n\
            id: e:6\revent: turn.started\rdata: line1\rdata:line2\r\r";
        let want = vec![
            SseItem::Frame(SseFrame {
                id: None,
                event: "hello".into(),
                data: "{\"a\":1}".into(),
            }),
            SseItem::Comment("keep-alive".into()),
            SseItem::Id("e:5".into()),
            SseItem::Frame(SseFrame {
                id: Some("e:6".into()),
                event: "turn.started".into(),
                data: "line1\nline2".into(),
            }),
        ];
        for chunk in [stream.len(), 1, 2, 3, 7] {
            let mut d = SseDecoder::new();
            let got: Vec<_> = stream.chunks(chunk).flat_map(|c| d.push(c)).collect();
            assert_eq!(got, want, "chunks of {chunk}");
            assert_eq!(d.last_event_id(), "e:6");
        }
    }

    #[test]
    fn a_record_without_an_event_name_is_a_message() {
        let mut d = SseDecoder::new();
        assert_eq!(
            d.push(b"data\n\n"),
            vec![SseItem::Frame(SseFrame {
                id: None,
                event: "message".into(),
                data: String::new(),
            })]
        );
        assert!(d.push(b"event: x\n\n").is_empty(), "no data: no record");
    }

    #[test]
    fn a_newer_gateway_s_events_never_stop_the_reader() {
        use lmgw_api_types::chat::KbMode;
        let mut r = FeedReader::new(Some("ep:3:aa11"));
        let got = r.push(
            b"id: ep:4:bb22\nevent: message.appended\ndata: {\"thread_id\":7}\n\n\
              id: ep:5:cc33\nevent: thread.updated\ndata: {\"id\":7,\"kb_mode\":\"hybrid\",\"by\":null}\n\n\
              id: ep:6:dd44\nevent: thread.updated\ndata: [1]\n\n",
        );
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(
            got[0],
            FeedItem::Unknown {
                event: "message.appended".into(),
                data: r#"{"thread_id":7}"#.into()
            },
            "a future event type"
        );
        let FeedItem::Event(FeedEvent::ThreadUpdated(ThreadNow::Row(t))) = &got[1] else {
            panic!("a future enum value reads: {:?}", got[1])
        };
        assert_eq!(t.thread.kb_mode, KbMode::Unknown("hybrid".into()));
        assert_eq!(
            got[2],
            FeedItem::Unreadable {
                event: "thread.updated".into(),
                data: "[1]".into()
            },
            "a stored event that does not read: the client reloads (review F-13)"
        );
        assert_eq!(
            r.cursor(),
            Some("ep:6:dd44"),
            "past each of them, and the client was told of each"
        );
    }

    #[test]
    fn the_reader_types_events_and_keeps_the_cursor() {
        let mut r = FeedReader::new(Some("e:3:0a1b"));
        assert_eq!(r.cursor(), Some("e:3:0a1b"));
        let hello = serde_json::json!({"epoch": "e", "cursor": "e:4:77aa", "keepalive_s": 15,
            "retention_days": 7, "principal": {"kind": "device", "name": "desktop"},
            "hosts_label": null, "hold": {"active": false, "fallback_alias": null},
            "voice": [], "turns": []});
        let got = r.push(format!("event: hello\ndata: {hello}\n\n").as_bytes());
        let [FeedItem::Event(FeedEvent::Hello(h))] = got.as_slice() else {
            panic!("{got:?}")
        };
        assert_eq!(h.principal.by(), "device 'desktop'");
        assert_eq!((r.cursor(), r.keepalive_s()), (Some("e:4:77aa"), Some(15)));
        let got = r.push(
            b"event: turn.started\ndata: {\"thread_id\":7,\"by\":\"the dashboard\",\"voice\":false}\n\n\
              : keep-alive\nid: e:9:c0ffee01\n\n",
        );
        assert_eq!(
            got,
            vec![
                FeedItem::Event(FeedEvent::TurnStarted(TurnStarted {
                    thread_id: 7,
                    by: "the dashboard".into(),
                    voice: false
                })),
                FeedItem::KeepAlive,
                FeedItem::KeepAlive,
            ]
        );
        assert_eq!(
            r.cursor(),
            Some("e:9:c0ffee01"),
            "the keep-alive's id moved it"
        );
        let got = r.push(b"id: e:10:5eed\nevent: voice.ended\ndata: {\"thread_id\":7,\"by\":\"device 'phone'\",\"reason\":\"closed\"}\n\n");
        assert_eq!(
            got,
            vec![FeedItem::Event(FeedEvent::VoiceEnded(VoiceEnded {
                thread_id: 7,
                by: "device 'phone'".into(),
                reason: "closed".into(),
                taken_over_by: None
            }))]
        );
        assert_eq!(r.cursor(), Some("e:10:5eed"));
        let got = r.push(b"event: turn.done\ndata: nope\n\n");
        assert_eq!(
            got,
            vec![FeedItem::Unreadable {
                event: "turn.done".into(),
                data: "nope".into()
            }]
        );
        let req = r.resume();
        assert!(req
            .headers
            .iter()
            .any(|h| h.name == "Last-Event-ID" && h.value == "e:10:5eed"));
    }
}
