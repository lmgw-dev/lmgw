//! The Chat's live updates (chat-plane design D1): what any writer changed,
//! which the follower (`chat_sync`) reads again, and the GPU hold the voice
//! controls show. The Chat takes them from the source its host hands it
//! ([`ChatLive`], in context):
//!
//! - **The dashboard** hands it the shell's one stream, `/api/events`
//!   ([`ChatLive::Host`]): its `chat` frames (D11), posted into a
//!   [`crate::live::ChatInbox`], and its `vram` frame's hold and benchmark
//!   lease. The page opens no stream of its own: a browser keeps at most six
//!   connections to one host over HTTP/1.1, and a second stream per Chat tab
//!   would let three tabs take them all.
//! - **A host that hands none** (the sidebar, WP8) gets the Chat's own feed,
//!   `GET /chat/api/feed` ([`ChatLive::Feed`]), the stream a paired device
//!   follows, read off the page's transport (`lmgw_ui_kit::http::Http`) as
//!   SSE records: each event's names ([`said`]) go into one pending change.
//!   A `hello` says "read everything again", as a (re)connect always has.
//!   The page carries the cursor itself ([`resume`]): the last `id:` (or
//!   `hello`'s and `caught_up`'s cursor) goes out as `Last-Event-ID` on the
//!   next connection, so the catch-up after it names what changed while it
//!   was down, and a connection that ends is tried again after a wait that
//!   doubles while tries fail. A connection silent for two of `hello`'s
//!   keep-alive intervals is dead and is ended and tried again. A `400` to
//!   a cursor connects again from now; any other `4xx` stops the feed, and
//!   the page says so ([`FeedStopped`]). An event that does not read is a
//!   resync too: what it was about may be shown stale.
//!   Its hold is `hello`'s, every `hold` event's and every `state`'s. One
//!   stream per page: opened when the page mounts and the host's session is
//!   good ([`crate::ChatHost::locked`]), closed at once when the page goes or
//!   the session locks (not at its next event, so a left page holds no
//!   connection), and opened again by a login.

mod resume;
mod said;

use std::future::Future;

use futures::future::{select, Either};
use futures::StreamExt;
use leptos::prelude::*;
use lmgw_api_types::chat_feed::{FeedEvent, FeedHold, EVENTS};
use lmgw_api_types::ChatChanged;
use lmgw_ui_kit::http::sse::{Decoder, Record};
use lmgw_ui_kit::http::{Http, Read, Request, Response};

use resume::{Refused, Resume};

use crate::live::{ChatInbox, ChatPost};

/// Where the Chat's live updates come from: what its host hands it
/// ([`crate::ChatHost::live`], module doc).
#[derive(Clone, Copy)]
pub enum ChatLive {
    /// A stream of the host's own (the dashboard shell's `/api/events`):
    /// what it says changed, posted into an inbox made by
    /// [`crate::live::inbox`], and the GPU hold as it says it (`None` while
    /// not known). The host's stream has said the hold before the Chat
    /// mounts, so a hold not known means that stream is down (D17).
    Host {
        changes: ChatInbox,
        hold: Signal<Option<FeedHold>>,
    },
    /// The Chat's own feed, `/chat/api/feed`, one stream per page.
    Feed,
}

/// The page's read ends of its live updates, whichever the source. `Copy`.
#[derive(Clone, Copy)]
pub(crate) struct ChatUpdates {
    /// What changed, until the follower takes it.
    pub changes: ChatInbox,
    /// The GPU hold: the owner's hold and a benchmark run's lease. `None`
    /// while not known: before the source first said it, and once its stream
    /// ended.
    pub hold: Signal<Option<FeedHold>>,
    /// Whether a hold not known is said as what the hold would do
    /// (`chat_voice::state::Block::unknown`, D17). The host's stream said
    /// the hold before the page mounted, so not knowing it means that stream
    /// is down; the page's own feed says it in its first event, so not
    /// knowing it is the moment before that, and nothing is said.
    pub unknown_said: bool,
    /// Why the page's own feed stopped for good (a refusal asking again
    /// would not change); `None` while it follows, and for the host's
    /// stream, whose state the host shows.
    pub stopped: Signal<Option<String>>,
}

/// Take the live updates from the host's source and provide them. Call
/// once, in the Chat page, before anything that reads them.
pub(crate) fn provide() -> ChatUpdates {
    let host = crate::host::use_chat_host();
    let updates = match host.live {
        ChatLive::Host { changes, hold } => ChatUpdates {
            changes,
            hold,
            unknown_said: true,
            stopped: Signal::stored(None),
        },
        ChatLive::Feed => feed(lmgw_ui_kit::http::use_http(), host.locked),
    };
    provide_context(updates);
    updates
}

/// The page's live updates (`provide`'s).
pub(crate) fn use_chat_updates() -> ChatUpdates {
    expect_context::<ChatUpdates>()
}

/// The page's own feed (module doc), through `http`: open while the host's
/// session is good (`locked` false).
fn feed(http: Http, locked: Signal<bool>) -> ChatUpdates {
    let (changes, post) = crate::live::inbox();
    let (hold, set_hold) = signal(None);
    let stopped = RwSignal::new(None::<String>);
    // The open stream's stop: dropping the sender ends it.
    let stop = StoredValue::new(None::<futures::channel::oneshot::Sender<()>>);
    // `/chat/api/feed` needs a principal: opened while the session is good,
    // let go when it is not, opened again by a login (as `/api/events` is).
    Effect::new(move |prev: Option<bool>| {
        let now = locked.get();
        if now {
            stop.update_value(|s| drop(s.take()));
        } else if prev != Some(false) {
            let (tx, rx) = futures::channel::oneshot::channel();
            stop.update_value(|s| *s = Some(tx));
            stopped.set(None);
            open(http, rx, post, set_hold, stopped, locked);
        }
        now
    });
    on_cleanup(move || {
        stop.try_update_value(|s| drop(s.take()));
    });
    ChatUpdates {
        changes,
        hold: hold.into(),
        unknown_said: false,
        stopped: stopped.into(),
    }
}

/// The page's word that its own feed stopped ([`ChatUpdates::stopped`]): a
/// badge in the Chat's head, the reason in its tooltip. Nothing while the
/// feed follows, and on the dashboard.
#[component]
pub(crate) fn FeedStopped() -> impl IntoView {
    let stopped = use_chat_updates().stopped;
    move || {
        stopped.get().map(|why| {
            view! {
                <span
                    class="type-badge feed-stopped-badge"
                    data-feed-stopped=why.clone()
                    title=format!(
                        "Changes made elsewhere no longer show here until the page is \
                         reloaded: the change feed stopped ({why})",
                    )
                >
                    "not live"
                </span>
            }
        })
    }
}

/// The feed's route.
const FEED: &str = "/chat/api/feed";

/// The next connection's request: a stream, resumed from the cursor.
fn feed_request(resume: &Resume) -> Request {
    let req = Request::get(FEED).read(Read::Stream);
    match resume.cursor() {
        Some(id) => req.header("Last-Event-ID", id),
        None => req,
    }
}

/// How one connection's stream ended.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    /// lmgw ended it, it dropped, or it was aborted.
    Closed,
    /// Silent past [`Resume::dead_after_ms`]: given up on.
    Dead,
}

/// Read one connection's records to its end: each feed event (or why one
/// did not read) to `on_event`, every record to the cursor. `quiet(ms)` is
/// a timer: once `hello` said the keep-alive, a wait for the next chunk
/// longer than [`Resume::dead_after_ms`] ends the connection as dead
/// (dropping the stream cancels the request, `lmgw_ui_kit::http::ByteStream`).
/// Every chunk counts as life, a keep-alive comment included.
async fn read_connection<Q: Future<Output = ()>>(
    resp: Response,
    resume: &mut Resume,
    mut on_event: impl FnMut(Result<FeedEvent, String>),
    quiet: impl Fn(u64) -> Q,
) -> Ended {
    let mut chunks = resp.stream();
    let mut records = Decoder::default();
    resume.connecting();
    loop {
        let next = chunks.next();
        let chunk = match resume.dead_after_ms() {
            None => next.await,
            Some(ms) => match select(next, Box::pin(quiet(ms))).await {
                Either::Left((chunk, _)) => chunk,
                Either::Right(_) => return Ended::Dead,
            },
        };
        // Aborted (the page went), dropped, or ended.
        let Some(Ok(chunk)) = chunk else {
            return Ended::Closed;
        };
        for r in records.push(&chunk) {
            resume.saw(&r);
            if let Some(ev) = feed_event(&r) {
                match &ev {
                    Ok(FeedEvent::Hello(h)) => {
                        resume.heard_keepalive(h.keepalive_s);
                        resume.moved(&h.cursor);
                    }
                    Ok(FeedEvent::CaughtUp(c)) => resume.moved(&c.cursor),
                    _ => {}
                }
                on_event(ev);
            }
        }
    }
}

/// A record as a feed event; `None` for one the feed does not send (the
/// `EventSource` subscribed to [`EVENTS`] only).
fn feed_event(r: &Record) -> Option<Result<FeedEvent, String>> {
    EVENTS
        .contains(&r.event.as_str())
        .then(|| FeedEvent::parse(&r.event, &r.data).map_err(|e| e.to_string()))
}

/// Wait `ms` milliseconds.
async fn sleep(ms: u64) {
    let (tx, rx) = futures::channel::oneshot::channel::<()>();
    set_timeout(
        move || {
            let _ = tx.send(());
        },
        std::time::Duration::from_millis(ms),
    );
    let _ = rx.await;
}

/// The feed, connected again after every end, until `stop` resolves (its
/// sender dropped): then the open request is aborted at once. A refusal
/// asking again would not change ends it too, said on `stopped`.
fn open(
    http: Http,
    mut stop: futures::channel::oneshot::Receiver<()>,
    post: ChatPost,
    set_hold: WriteSignal<Option<FeedHold>>,
    stopped: RwSignal<Option<String>>,
    locked: Signal<bool>,
) {
    let on_event = move |ev: Result<FeedEvent, String>| match ev {
        Ok(ev) => {
            let s = said::said(ev);
            if let Some(c) = s.changed {
                post.post(c);
            }
            if let Some(h) = s.hold {
                set_hold.try_set(Some(h));
            }
        }
        Err(err) => {
            leptos::logging::warn!("{err}: reading everything again");
            post.post(ChatChanged::resync());
        }
    };
    leptos::task::spawn_local(async move {
        let mut resume = Resume::default();
        loop {
            let Ok(ctrl) = web_sys::AbortController::new() else {
                break;
            };
            let sent_cursor = resume.cursor().is_some();
            let req = feed_request(&resume).signal(Some(&ctrl.signal()));
            let connection = async {
                match http.send(req).await {
                    Ok(resp) if resp.ok() => {
                        if read_connection(resp, &mut resume, on_event, sleep).await == Ended::Dead
                        {
                            leptos::logging::warn!(
                                "{FEED}: silent past its keep-alive: reconnecting"
                            );
                        }
                        Refused::Retry
                    }
                    Ok(resp) => {
                        let status = resp.status;
                        // Read as every refusal is: a `401 session_required`
                        // runs the host's hook (the kit's), which locks its
                        // session, and the session check below ends the feed
                        // then.
                        let e = resp.refusal().await;
                        let verdict = resume::refused(status, sent_cursor);
                        leptos::logging::warn!("{FEED}: {status} {e}");
                        if verdict == Refused::Stop {
                            stopped.try_set(Some(format!("{status}: {e}")));
                        }
                        verdict
                    }
                    Err(e) => {
                        leptos::logging::warn!("{FEED}: {e}");
                        Refused::Retry
                    }
                }
            };
            let verdict = match select(&mut stop, Box::pin(connection)).await {
                Either::Left(_) => {
                    ctrl.abort();
                    break;
                }
                Either::Right((verdict, _)) => verdict,
            };
            // Ended for whatever reason: a request still open (a dead link,
            // a relay's stream) is let go.
            ctrl.abort();
            // A locked session does not fix itself: the login opens a new
            // feed. The host derives the signal, so it may be gone with
            // the host's owner: then the page is going too, and the loop
            // ends.
            if locked.try_get_untracked().unwrap_or(true) {
                break;
            }
            match verdict {
                Refused::Stop => break,
                Refused::Fresh => {
                    resume.drop_cursor();
                    continue;
                }
                Refused::Retry => {}
            }
            let wait = resume.next_wait_ms();
            if let Either::Left(_) = select(&mut stop, Box::pin(sleep(wait))).await {
                break;
            }
        }
        set_hold.try_set(None);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The feed over a fake transport: two connections, the second
    /// resuming from the first's last id.
    #[test]
    fn the_feed_resumes_from_its_cursor_after_the_stream_ends() {
        use futures::executor::block_on;
        use lmgw_ui_kit::http::fake::Fake;

        let fake = Fake::new(|req| {
            Ok(match req.header_value("last-event-id") {
                // The first connection: an event split across chunks, one
                // the feed does not send (skipped, its id still counted),
                // then the stream ends.
                None => Fake::chunks(
                    200,
                    vec![
                        b": keep-alive\n\nid: e1:4\nevent: thread.upd",
                        b"ated\ndata: {\"thread_id\":7,\"deleted\":true,\"by\":null}\n\n",
                        b"id: e1:5\nevent: not.a.feed.event\ndata: {}\n\n",
                    ],
                ),
                // The reconnect resumes there.
                Some("e1:5") => Fake::chunks(
                    200,
                    vec![b"id: e1:6\nevent: folder.updated\ndata: {\"id\":3,\"name\":\"f\",\"by\":null}\n\n"],
                ),
                Some(other) => panic!("resumed from {other}"),
            })
        });
        let http = Http::new(fake.clone());
        let mut resume = Resume::default();
        let mut seen = Vec::new();
        for _ in 0..2 {
            let req = feed_request(&resume);
            let resp = block_on(http.ok(req)).unwrap();
            let ended = block_on(read_connection(
                resp,
                &mut resume,
                |ev| {
                    seen.push(said::said(ev.unwrap()).changed.unwrap());
                },
                |_| futures::future::pending::<()>(),
            ));
            assert_eq!(ended, Ended::Closed);
            // Each connection delivered: the next try waits the first wait.
            assert_eq!(resume.next_wait_ms(), resume::FIRST_WAIT_MS);
        }
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].threads, [7]);
        assert_eq!(seen[1].folders, [3]);
        let sent = fake.sent();
        assert_eq!(sent.len(), 2);
        assert!(sent
            .iter()
            .all(|r| r.path == FEED && r.read == Read::Stream));
        assert_eq!(sent[0].header_value("last-event-id"), None);
        assert_eq!(sent[1].header_value("last-event-id"), Some("e1:5"));
        assert_eq!(resume.cursor(), Some("e1:6"));
    }

    /// A relay whose far side went away: the stream neither ends nor
    /// sends. Two of `hello`'s keep-alive intervals end it, and the
    /// reconnect resumes from `hello`'s cursor.
    #[test]
    fn a_silent_stream_is_dead_after_two_keepalives() {
        use futures::executor::block_on;
        use lmgw_ui_kit::http::fake::Fake;
        use std::cell::RefCell;
        use std::rc::Rc;

        let http = Http::new(Fake::new(|_| {
            let hello: &[u8] = b"event: hello\ndata: {\"epoch\":\"e\",\"cursor\":\"e:3:aa\",\"keepalive_s\":15}\n\n";
            let chunks: Vec<Result<Vec<u8>, String>> =
                vec![Ok(b": keep-alive\n\n".to_vec()), Ok(hello.to_vec())];
            Ok(Response::streamed(
                200,
                vec![],
                futures::stream::iter(chunks)
                    .chain(futures::stream::pending())
                    .boxed_local(),
            ))
        }));
        let mut resume = Resume::default();
        let waited = Rc::new(RefCell::new(Vec::new()));
        let w = waited.clone();
        let resp = block_on(http.ok(feed_request(&resume))).unwrap();
        let mut events = 0;
        let ended = block_on(read_connection(
            resp,
            &mut resume,
            |_| events += 1,
            move |ms| {
                w.borrow_mut().push(ms);
                futures::future::ready(())
            },
        ));
        assert_eq!(ended, Ended::Dead);
        assert_eq!(events, 1, "hello");
        // Before hello no clock ran; after it, two keep-alives of 15 s.
        assert_eq!(*waited.borrow(), [30_000]);
        assert_eq!(resume.cursor(), Some("e:3:aa"));
        assert_eq!(
            feed_request(&resume).header_value("last-event-id"),
            Some("e:3:aa")
        );
    }

    /// A cursor the feed cannot read is a `400`: the next connection goes
    /// without it, and its `hello` makes the page read everything again.
    #[test]
    fn a_refused_cursor_is_dropped_and_the_feed_connects_from_now() {
        use futures::executor::block_on;
        use lmgw_ui_kit::http::fake::Fake;

        let fake = Fake::new(|req| {
            Ok(match req.header_value("last-event-id") {
                Some(_) => Fake::json(
                    400,
                    r#"{"code":"bad_request","message":"not a feed cursor"}"#,
                ),
                None => Fake::chunks(
                    200,
                    vec![b"event: hello\ndata: {\"cursor\":\"e:9:00\",\"keepalive_s\":15}\n\n"],
                ),
            })
        });
        let http = Http::new(fake.clone());
        let mut resume = Resume::default();
        resume.moved("from another database");
        let sent_cursor = resume.cursor().is_some();
        let resp = block_on(http.send(feed_request(&resume))).unwrap();
        assert_eq!(resume::refused(resp.status, sent_cursor), Refused::Fresh);
        assert_eq!(block_on(resp.refusal()).to_string(), "not a feed cursor");
        resume.drop_cursor();
        let resp = block_on(http.ok(feed_request(&resume))).unwrap();
        let mut hello = false;
        block_on(read_connection(
            resp,
            &mut resume,
            |ev| hello = matches!(ev, Ok(FeedEvent::Hello(_))),
            |_| futures::future::pending::<()>(),
        ));
        assert!(hello);
        assert!(said::said(FeedEvent::Hello(Default::default()))
            .changed
            .is_some_and(|c| c.resync));
        let sent = fake.sent();
        assert_eq!(
            sent[0].header_value("last-event-id"),
            Some("from another database")
        );
        assert_eq!(sent[1].header_value("last-event-id"), None);
        assert_eq!(resume.cursor(), Some("e:9:00"));
    }

    #[test]
    fn a_feed_that_cannot_connect_waits_longer_each_time() {
        use futures::executor::block_on;
        use lmgw_ui_kit::http::fake::Fake;

        let http = Http::new(Fake::new(|_| Err("connection refused".into())));
        let mut resume = Resume::default();
        let mut waits = Vec::new();
        for _ in 0..3 {
            assert!(block_on(http.ok(feed_request(&resume))).is_err());
            waits.push(resume.next_wait_ms());
        }
        assert_eq!(waits, [1_000, 2_000, 4_000]);
        // A refusal is no event: nothing resets the wait.
        let http = Http::new(Fake::new(|_| {
            Ok(Fake::json(503, r#"{"code":"x","message":"down"}"#))
        }));
        assert!(block_on(http.ok(feed_request(&resume))).is_err());
        assert_eq!(resume.next_wait_ms(), 8_000);
    }

    #[test]
    fn a_record_that_does_not_read_is_a_resync() {
        let bad = Record {
            event: "thread.updated".into(),
            data: "not json".into(),
            ..Record::default()
        };
        assert!(matches!(feed_event(&bad), Some(Err(_))));
        let other = Record {
            event: "message".into(),
            data: "{}".into(),
            ..Record::default()
        };
        assert!(feed_event(&other).is_none());
    }
}
