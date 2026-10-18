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
//! arrives is a sign of life. A client may ask for its own interval
//! ([`crate::requests::feed`]'s `keepalive_s`, [`FeedReader::asking`]), as a
//! phone holding the feed in the background does.
//!
//! **The open approvals** (device link design §7.3): [`FeedReader`] keeps
//! the calls waiting for a decision ([`FeedReader::open_approvals`]) and
//! yields each change as [`FeedItem::Approvals`]: the whole set at
//! `caught_up` ([`ApprovalsChange::Open`]), then what is added and
//! withdrawn, by `approval.*` events and by the set every `state` carries
//! again. A reader lives for one connection, so `Open` comes again at
//! every reconnect and is the set to reconcile what is shown against; a
//! reader made with [`FeedReader::knowing`] does that itself (`open`'s
//! module doc).
//!
//! **Messages** (client-apps design §2.2, WP10): `message.added` and
//! `message.updated` carry the messages whole, as
//! `GET /chat/api/threads/{id}` lists them, with the thread's title
//! ([`MessagesChanged`]), so a client keeps an open conversation current,
//! or builds a notification, from the event alone; `message.deleted` names
//! the ids that went ([`MessagesDeleted`]). Apply them by message id: two
//! events may carry the same state, and a catch-up renders each as it is
//! now.
//!
//! **A turn's stream** (`POST /chat/api/threads/{id}/send` and the other
//! turn routes) is SSE too: [`SseDecoder`] decodes it, and
//! `lmgw_api_types::chat_frames::TurnEvent::parse` types each frame by its
//! name (`turn`, `delta`, `tool`, `done`, …).

pub use lmgw_api_types::chat_feed::{
    ApprovalDecided, ApprovalRequested, CaughtUp, Cursor, DeviceRevoked, FeedEvent, FeedEventError,
    FeedPrincipal, FeedProfile, FolderNow, Hello, MessagesChanged, MessagesDeleted, TaskDone,
    TaskStarted, ThreadNow, EVENTS,
};

mod open;
pub use open::ApprovalsChange;

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
    /// The open approvals changed (module doc), right after the event that
    /// changed them.
    Approvals(ApprovalsChange),
}

/// The feed of one connection: typed events, the cursor to resume from
/// after it ends, and the calls waiting for a decision (module doc).
#[derive(Debug, Clone, Default)]
pub struct FeedReader {
    sse: SseDecoder,
    cursor: Option<String>,
    keepalive_s: Option<u32>,
    /// The keep-alive the connection asked for, asked again by
    /// [`Self::resume`].
    asked: Option<u32>,
    /// The calls waiting for a decision (`open`).
    approvals: open::OpenSet,
    /// What `hello` said about the device itself (device link design §7.2).
    device: HelloFacts,
}

/// `hello`'s facts about the device, as the client keeps them.
#[derive(Debug, Clone, Default)]
struct HelloFacts {
    urls: Option<crate::pairing::Urls>,
    cert: Option<crate::requests::CertDates>,
    dashboard_url: Option<String>,
}

impl FeedReader {
    /// A reader for a connection opened with `resume` (`None`: from now).
    pub fn new(resume: Option<&str>) -> Self {
        FeedReader {
            cursor: resume.filter(|c| !c.is_empty()).map(str::to_string),
            ..Default::default()
        }
    }

    /// The same reader for a connection that asked for a keep-alive every
    /// `keepalive_s` seconds ([`crate::requests::feed`]): [`Self::resume`]
    /// asks for it again.
    pub fn asking(mut self, keepalive_s: Option<u32>) -> Self {
        self.asked = keepalive_s;
        self
    }

    /// The same reader for a client that already shows, or was told to
    /// forget, the approvals `known` (by `approval_request_id`: the ones it
    /// notified for on an earlier connection and the ones the user
    /// dismissed). At `caught_up` it yields, instead of the whole set
    /// ([`ApprovalsChange::Open`]), [`ApprovalsChange::Withdrawn`] with the
    /// known ids the set lacks (decided while the link was down) and
    /// [`ApprovalsChange::Added`] for each call of the set it does not
    /// know, so a reconnect neither re-raises nor leaves a stale
    /// notification.
    pub fn knowing(mut self, known: impl IntoIterator<Item = String>) -> Self {
        self.approvals.known = Some(known.into_iter().collect());
        self
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
                                self.device = HelloFacts {
                                    urls: crate::pairing::refresh_urls(&h.urls),
                                    cert: crate::requests::CertDates::of_hello(h.cert.as_ref()),
                                    dashboard_url: h.dashboard_url.clone(),
                                };
                                self.moved(h.cursor.clone());
                            }
                            if let FeedEvent::CaughtUp(c) = &ev {
                                // Every stored event up to here was
                                // delivered or passed: a cursor to resume
                                // from, as a keep-alive's `id:` is.
                                self.moved(c.cursor.clone());
                            }
                            let changes = self.approvals.apply(&ev);
                            out.push(FeedItem::Event(ev));
                            out.extend(changes.into_iter().map(FeedItem::Approvals));
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

    /// The calls waiting for a decision, oldest first (module doc):
    /// `hello`'s, changed by the events after `caught_up`.
    pub fn open_approvals(&self) -> &[ApprovalRequested] {
        &self.approvals.open
    }

    /// Whether `caught_up` came on this connection: the catch-up is over.
    pub fn caught_up(&self) -> bool {
        self.approvals.caught_up
    }

    /// The cursor to resume from: `Last-Event-ID` of the next connection.
    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    /// `hello.keepalive_s`, once `hello` came.
    pub fn keepalive_s(&self) -> Option<u32> {
        self.keepalive_s
    }

    /// `hello.urls` as the device keeps its list ([`refresh_urls`]: the
    /// `https` ones, each once); `None` before `hello`, and when it holds
    /// none usable: the client keeps the list it has.
    ///
    /// [`refresh_urls`]: crate::pairing::refresh_urls
    pub fn urls(&self) -> Option<&crate::pairing::Urls> {
        self.device.urls.as_ref()
    }

    /// `hello.cert`, once `hello` came with one: the dates the app shows
    /// the certificate's notice from ([`crate::requests::CertDates::state`]).
    pub fn certificate(&self) -> Option<crate::requests::CertDates> {
        self.device.cert
    }

    /// `hello.dashboard_url`: where lmgw's dashboard is for this device,
    /// when lmgw offers one ("Open in lmgw").
    pub fn dashboard_url(&self) -> Option<&str> {
        self.device.dashboard_url.as_deref()
    }

    /// The request that resumes this feed, asking for the keep-alive this
    /// connection asked for ([`Self::asking`]).
    pub fn resume(&self) -> crate::requests::Request {
        crate::requests::feed(self.cursor(), self.asked)
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

    /// Personality profiles (personality-profiles design §3.2): a stored
    /// `profile.*` moves the cursor; the list after a `resync` comes as
    /// `profile.created` without an `id:`, and leaves it where it is.
    #[test]
    fn profile_events_read_with_and_without_a_cursor() {
        let mut r = FeedReader::new(Some("e:3:0a1b"));
        let got = r.push(
            b"id: e:4:aa\nevent: profile.updated\ndata: {\"id\":3,\"name\":\"Calm\",\"by\":\"the dashboard\"}\n\n\
              event: resync\ndata: {\"reason\":\"x\"}\n\n\
              event: profile.created\ndata: {\"id\":3,\"name\":\"Calm\",\"by\":null}\n\n",
        );
        let calm = |by: Option<&str>| FeedProfile {
            id: 3,
            name: "Calm".into(),
            by: by.map(str::to_string),
        };
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(
            got[0],
            FeedItem::Event(FeedEvent::ProfileUpdated(calm(Some("the dashboard"))))
        );
        assert_eq!(
            got[2],
            FeedItem::Event(FeedEvent::ProfileCreated(calm(None)))
        );
        assert_eq!(r.cursor(), Some("e:4:aa"));
    }

    /// `message.*` (WP10): stored events, typed, that move the cursor; the
    /// messages come whole with the thread's title, a delete as the ids
    /// that went.
    #[test]
    fn message_events_carry_the_messages_whole_or_their_ids() {
        let mut r = FeedReader::new(Some("e:20:aa"));
        let got = r.push(
            b"id: e:21:bb\nevent: message.added\ndata: {\"thread_id\":7,\"thread_title\":\"Kai\",\"by\":\"the dashboard\",\"messages\":[{\"id\":41,\"thread_id\":7,\"role\":\"assistant\",\"content\":\"It is late.\",\"reasoning\":\"\",\"kb_refs\":[],\"attachments\":[],\"created_at\":\"2026-10-14 20:00:00\"}]}\n\n\
              id: e:22:cc\nevent: message.deleted\ndata: {\"thread_id\":7,\"message_ids\":[41],\"by\":null}\n\n",
        );
        let [FeedItem::Event(FeedEvent::MessageAdded(added)), FeedItem::Event(FeedEvent::MessageDeleted(gone))] =
            got.as_slice()
        else {
            panic!("{got:?}")
        };
        assert_eq!((added.thread_id, added.thread_title.as_str()), (7, "Kai"));
        assert_eq!(added.by.as_deref(), Some("the dashboard"));
        let m = &added.messages[0];
        assert_eq!(
            (m.id, m.role.as_str(), m.content.as_str()),
            (41, "assistant", "It is late.")
        );
        assert_eq!(
            gone,
            &MessagesDeleted {
                thread_id: 7,
                message_ids: vec![41],
                by: None,
            }
        );
        assert_eq!(r.cursor(), Some("e:22:cc"));
    }

    /// A deleted device (MCP Tasks design §4.1): a stored event, typed,
    /// that moves the cursor; its device reads as a principal, whose `by`
    /// is what that device's changes carried.
    #[test]
    fn a_device_revoked_event_moves_the_cursor_and_names_the_device() {
        let mut r = FeedReader::new(None);
        let got = r.push(
            b"id: e:12:ab\nevent: device.revoked\ndata: {\"device\":{\"kind\":\"device\",\"name\":\"phone\"},\"by\":\"the dashboard\"}\n\n",
        );
        let [FeedItem::Event(FeedEvent::DeviceRevoked(d))] = got.as_slice() else {
            panic!("{got:?}")
        };
        assert_eq!(
            d,
            &DeviceRevoked {
                device: FeedPrincipal {
                    kind: "device".into(),
                    name: "phone".into(),
                },
                by: Some("the dashboard".into()),
            }
        );
        assert_eq!(d.device.by(), "device 'phone'");
        assert_eq!(r.cursor(), Some("e:12:ab"));
    }

    #[test]
    fn hello_s_device_facts_are_read() {
        use crate::requests::CredentialState::{Lapsed, Notice, Valid};
        let mut r = FeedReader::new(None);
        assert_eq!(
            (r.urls(), r.certificate(), r.dashboard_url()),
            (None, None, None)
        );
        let hello = serde_json::json!({"epoch": "e", "cursor": "e:4:77aa", "keepalive_s": 15,
            "retention_days": 7, "principal": {"kind": "device", "name": "desktop"},
            "urls": ["http://h:1", "https://192.0.2.1:8788/", "https://192.0.2.1:8788"],
            "cert": {"not_after": "2027-10-14T14:03:11Z", "notice_after": "2027-06-14T14:03:11Z"},
            "dashboard_url": "http://127.0.0.1:8001/"});
        r.push(format!("event: hello\ndata: {hello}\n\n").as_bytes());
        assert_eq!(r.urls().unwrap().as_slice(), ["https://192.0.2.1:8788"]);
        let c = r.certificate().unwrap();
        assert_eq!(c.not_after_ms, 1_823_522_591_000);
        assert_eq!(c.state(c.notice_after_ms - 1), Valid);
        assert_eq!(c.state(c.notice_after_ms), Notice);
        assert_eq!(c.state(c.not_after_ms), Lapsed);
        assert_eq!(r.dashboard_url(), Some("http://127.0.0.1:8001/"));
        // A later connection's `hello` without them replaces them.
        let bare = serde_json::json!({"epoch": "e", "cursor": "e:4:77aa", "keepalive_s": 15,
            "urls": ["http://only-plain"], "cert": null, "dashboard_url": null});
        r.push(format!("event: hello\ndata: {bare}\n\n").as_bytes());
        assert_eq!(
            (r.urls(), r.certificate(), r.dashboard_url()),
            (None, None, None)
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
        assert_eq!(req.path, "/chat/api/feed", "asked for no keep-alive");
    }

    /// Device link §7.1: a reader for a feed that asked for its keep-alive
    /// asks for it again when it resumes.
    #[test]
    fn a_resume_asks_for_the_same_keepalive() {
        let r = FeedReader::new(Some("e:3:aa")).asking(Some(120));
        let req = r.resume();
        assert_eq!(req.path, "/chat/api/feed?keepalive_s=120");
        assert!(req
            .headers
            .iter()
            .any(|h| h.name == "Last-Event-ID" && h.value == "e:3:aa"));
    }
}
