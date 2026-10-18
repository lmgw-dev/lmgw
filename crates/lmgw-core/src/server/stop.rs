//! The long-lived streams end when their server stops, and a stopping server
//! waits for what it started, within one bound.
//!
//! axum's graceful shutdown stops accepting, then waits for every open
//! connection to finish. A response that never finishes by itself held it
//! for as long as its client stayed: the Chat change feed, a Chat turn's
//! SSE, `/mcp`'s notification stream, the dashboard's `/api/events`, an
//! agent app's event stream through the agent proxy. A headless gateway's
//! Ctrl-C then waited on them, and a restart left the old server serving
//! them. Each now watches [`Stops`] and ends when the server it came in on
//! stops; a realtime session (an upgraded connection the drain does not wait
//! for) closes with 1001.
//!
//! A generation, not a flag: the shell's "Restart gateway" stops one server
//! and starts the next on the same state, and a stream opened on the new one
//! must not end for the old one's stop. **A request carries its server's
//! generation from the moment it arrives** ([`ServedAt`], review F-3), not
//! from when its stream is built: a feed still reading its catch-up, or an
//! upgrade still in its handshake, when the stop comes, ends with the others.
//!
//! **What a stopping server waits for.** The drain does not wait for an
//! upgraded connection, and a Chat turn runs on a task of its own: a headless
//! gateway that exits as soon as the server returns reset a realtime session
//! mid-close (its client read 1006), and dropped a cut turn's partial save
//! and its request row. Each holds a [`Running`] until it has ended — a
//! session its close sent and its bound thread's journal written, a turn
//! (a Chat turn, a realtime response's model call or a bound voice turn) its
//! reply saved and its row written — and so does a request row's write
//! ([`Stops::writing`]). The server waits for the ones opened on it
//! ([`Stops::ended`]) before it returns.
//!
//! **One bound for all of it** ([`STOP_WITHIN`], review F-4): from the stop,
//! the drain and then that wait share it, and what was still open when it
//! ran out is said in the log ([`Stops::open_requests`]). A stop never waits
//! longer, whatever a client or an upstream does.
//!
//! **A generation per listener** (device link design §4.4): the plain
//! listener and the device listener each have their own, and a request
//! carries its listener's ([`ServedAt::via`]). Turning the device listener
//! off or moving it ends its streams, sessions and calls, never the plain
//! listener's; a gateway stop stops both, within the one bound.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use futures::stream::{BoxStream, StreamExt};
use futures::Stream;
use tokio::sync::watch;

use super::conn::Via;
use crate::state::SharedState;

/// The close code a realtime session ends with when its server stops:
/// RFC 6455's "going away". Shared with clients.
pub use lmgw_api_types::realtime::CLOSE_GOING_AWAY;

/// The sentence a stream that ends for the stop says it with, where it has a
/// frame to say it in: the 1001 close's reason, a Chat stream's last
/// `error`. Shared with clients.
pub use lmgw_api_types::realtime::SHUTTING_DOWN as STOPPING;

/// How long a stopping server takes at most, from its stop to its return:
/// the drain of its open responses, then the wait for its sessions and
/// turns ([`Stops::ended`]). Whatever is still open then is said in the log
/// and left behind. A session ends within moments of the stop and a cut
/// turn saves at once; this bounds a client that stopped reading and an
/// upstream that stopped answering.
pub const STOP_WITHIN: Duration = Duration::from_secs(10);

/// The bound's earlier name, for the suites that wait on a stop.
pub const SESSIONS_END_WITHIN: Duration = STOP_WITHIN;

/// The listener a request came in on, and that listener's generation then:
/// put on every request by the serving loop, before anything else reads it
/// (review F-3). A stream or a session it opens ends at that listener's stop,
/// however long it took to get going, and never at the other listener's
/// (device link design §4.4). Read through `RequestCtx::served_at`, or from
/// the extensions where no context is resolved (the agent proxy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServedAt {
    pub via: Via,
    pub at: u64,
}

/// The listeners' stops (device link design §4.4): per listener, a
/// generation that moves on each time it stops; the sessions and turns
/// running per listener and generation they were opened at; the request
/// rows being written; and the requests in flight.
///
/// One per gateway, holding both listeners' generations: the plain
/// listener's moves when a gateway stop (or a restart) stops it, the device
/// listener's when it is turned off or moved, and at a gateway stop. A
/// stream watches the one its request came in on.
#[derive(Debug)]
pub struct Stops {
    plain: watch::Sender<u64>,
    devices: watch::Sender<u64>,
    running: Arc<watch::Sender<BTreeMap<Option<ServedAt>, usize>>>,
    open: Arc<std::sync::Mutex<Open>>,
}

/// The requests in flight, by a number of their own: what a drain that ran
/// out of time names.
#[derive(Debug, Default)]
struct Open {
    next: u64,
    by_id: BTreeMap<u64, (ServedAt, String)>,
}

impl Default for Stops {
    fn default() -> Self {
        Self {
            plain: watch::Sender::new(0),
            devices: watch::Sender::new(0),
            running: Arc::new(watch::Sender::new(BTreeMap::new())),
            open: Arc::default(),
        }
    }
}

/// A session or a turn running, from its start to its end
/// ([`Stops::running_at`]).
#[derive(Debug)]
pub struct Running {
    /// `None`: a request row being written ([`Stops::writing`]).
    at: Option<ServedAt>,
    running: Arc<watch::Sender<BTreeMap<Option<ServedAt>, usize>>>,
}

/// One request in flight ([`Stops::request`]): gone when its response has
/// been sent whole, or dropped.
#[derive(Debug)]
pub struct InFlight {
    id: u64,
    open: Arc<std::sync::Mutex<Open>>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_id
            .remove(&self.id);
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.running.send_modify(|m| {
            if let Some(n) = m.get_mut(&self.at) {
                *n -= 1;
                if *n == 0 {
                    m.remove(&self.at);
                }
            }
        });
    }
}

impl Stops {
    fn lane(&self, via: Via) -> &watch::Sender<u64> {
        match via {
            Via::Plain => &self.plain,
            Via::Devices => &self.devices,
        }
    }

    /// The plain listener's generation now: what a stream or a turn with no
    /// request of its own (in-process, the dashboard's own work) is opened
    /// at.
    pub fn now(&self) -> ServedAt {
        self.now_of(Via::Plain)
    }

    /// `via`'s generation now: a stream opened now on it ends at its next
    /// stop.
    pub fn now_of(&self, via: Via) -> ServedAt {
        ServedAt {
            via,
            at: *self.lane(via).borrow(),
        }
    }

    /// The plain listener stopped ([`Self::stop_of`]).
    pub fn stop(&self) -> ServedAt {
        self.stop_of(Via::Plain)
    }

    /// `via` stopped: every stream opened on it before this ends; the other
    /// listener's go on. The generation it stopped: what [`Self::ended`]
    /// waits on.
    pub fn stop_of(&self, via: Via) -> ServedAt {
        let mut stopped = 0;
        self.lane(via).send_modify(|g| {
            stopped = *g;
            *g += 1;
        });
        ServedAt { via, at: stopped }
    }

    /// A session or a turn starts now on the plain listener: held until it
    /// has ended.
    pub fn running(&self) -> Running {
        self.running_at(self.now())
    }

    /// A session or a turn of a request that came in at `at`
    /// ([`ServedAt`]): held until it has ended. That listener's stop waits
    /// for it.
    pub fn running_at(&self, at: ServedAt) -> Running {
        self.held(Some(at))
    }

    fn held(&self, at: Option<ServedAt>) -> Running {
        self.running.send_modify(|m| *m.entry(at).or_default() += 1);
        Running {
            at,
            running: self.running.clone(),
        }
    }

    /// Work every stop waits for, whichever listener it belongs to: a
    /// request row being written ([`Self::ended`]).
    pub fn writing(&self) -> Running {
        self.held(None)
    }

    /// `at` when a request carried it, else the plain listener's generation
    /// now: what a stream or a turn without a request's generation is
    /// opened at.
    pub fn at_or_now(&self, at: Option<ServedAt>) -> ServedAt {
        at.unwrap_or_else(|| self.now())
    }

    /// Wait, at most `within`, until every session and turn opened on
    /// `stopped.via` at `stopped.at` or before has ended, and every request
    /// row being written; how many still run.
    pub async fn ended(&self, stopped: ServedAt, within: Duration) -> usize {
        let mut rx = self.running.subscribe();
        let open = |m: &BTreeMap<Option<ServedAt>, usize>| {
            m.iter()
                .filter(|(k, _)| k.is_none_or(|k| k.via == stopped.via && k.at <= stopped.at))
                .map(|(_, n)| n)
                .sum::<usize>()
        };
        let _ = tokio::time::timeout(within, rx.wait_for(|m| open(m) == 0)).await;
        let n = open(&rx.borrow());
        n
    }

    /// The sessions and turns running on `via` now, of every generation.
    pub fn running_on(&self, via: Via) -> usize {
        self.running
            .borrow()
            .iter()
            .filter(|(k, _)| k.is_some_and(|k| k.via == via))
            .map(|(_, n)| n)
            .sum()
    }

    /// A request came in at `at`, as `what` ("GET /chat/api/feed"): in
    /// flight until the guard drops.
    pub fn request(&self, at: ServedAt, what: String) -> InFlight {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        open.next += 1;
        let id = open.next;
        open.by_id.insert(id, (at, what));
        InFlight {
            id,
            open: self.open.clone(),
        }
    }

    /// The requests that came in at `at` and are still in flight, as they
    /// came in ("GET /chat/api/feed"), oldest first.
    pub fn open_requests(&self, at: ServedAt) -> Vec<String> {
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_id
            .values()
            .filter(|(g, _)| *g == at)
            .map(|(_, what)| what.clone())
            .collect()
    }

    /// Resolves once `opened_at`'s listener stopped after `opened_at`.
    pub fn stopped_after(&self, opened_at: ServedAt) -> impl Future<Output = ()> + Send + 'static {
        let mut rx = self.lane(opened_at.via).subscribe();
        async move {
            // The sender lives in the state for as long as any stream does;
            // a closed channel is a state that went, and nothing waits on it.
            let _ = rx.wait_for(|g| *g > opened_at.at).await;
        }
    }
}

/// `events` until the listener of `at` stops (`None`: the plain listener
/// serving now — a stream with no request's [`ServedAt`]), then `last` (the
/// frame that says so, if the stream has one) and the end. Dropping the
/// stream drops what produced `events`, as a client that goes away does.
pub fn until_stopped<T, S>(
    stops: &Stops,
    at: Option<ServedAt>,
    events: S,
    last: Option<T>,
) -> BoxStream<'static, T>
where
    T: Send + 'static,
    S: Stream<Item = T> + Send + 'static,
{
    let stopped: futures::future::BoxFuture<'static, ()> =
        Box::pin(stops.stopped_after(stops.at_or_now(at)));
    futures::stream::unfold(
        (events.boxed(), Some((stopped, last))),
        |(mut events, pending)| async move {
            let (mut stopped, last) = pending?;
            tokio::select! {
                biased;
                () = &mut stopped => last.map(|frame| (frame, (events, None))),
                item = events.next() => item.map(|item| (item, (events, Some((stopped, last))))),
            }
        },
    )
    .boxed()
}

/// `server::serve_app_within`'s body: serve `app` on `listener`, every
/// request carrying the plain listener's generation and counted in flight;
/// at `shutdown`, the stop (module doc), all of it within `within`.
pub(super) async fn serve_stopping<L>(
    state: &SharedState,
    listener: L,
    app: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    within: std::time::Duration,
) -> anyhow::Result<()>
where
    L: axum::serve::Listener<Addr = std::net::SocketAddr>,
    for<'a> super::Conn: axum::extract::connect_info::Connected<axum::serve::IncomingStream<'a, L>>,
{
    // This server's generation, put on every request before anything reads
    // it (review F-3): a stream or a session ends at this server's stop
    // however long it took to open, and the stop waits for the turns its
    // requests started. Outermost, so even the agent proxy (above the
    // principal) sees it, and every request is counted in flight.
    let served_at = state.stops.now_of(Via::Plain);
    let app = stamped(state, app, served_at);
    let st = state.clone();
    let stopped = std::sync::Arc::new(std::sync::OnceLock::new());
    let at = stopped.clone();
    let shutdown = async move {
        shutdown.await;
        tracing::info!(
            "the server stops: its open streams end and its sessions close, then it drains \
             and waits for its sessions and turns (at most {} s)",
            within.as_secs_f64()
        );
        let _ = at.set((st.stops.stop_of(Via::Plain), tokio::time::Instant::now()));
    };
    // With the connection on every request (`Conn`): the agent proxy tells
    // an app who reached lmgw in `X-Forwarded-For` (origins §4.6), and that
    // address is the TCP peer's, never a header a client wrote; a call's
    // candidate is the local address (calls §3.2).
    let drain = std::future::IntoFuture::into_future(
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<super::Conn>(),
        )
        .with_graceful_shutdown(shutdown),
    );
    // The drain is bounded too (review F-4): a response that ends neither
    // by itself nor at the stop — an upstream that stopped answering — does
    // not hold the stop. Counted from the stop, not from the start.
    let out_of_time = async {
        state.stops.stopped_after(served_at).await;
        tokio::time::sleep(within).await;
    };
    tokio::select! {
        served = drain => served?,
        () = out_of_time => {
            let open = state.stops.open_requests(served_at);
            tracing::warn!(
                "the drain did not finish within {} s of the stop; the server returns without \
                 {} request(s) still open: {}",
                within.as_secs_f64(),
                open.len(),
                if open.is_empty() { "(none counted)".to_string() } else { open.join(", ") }
            );
        }
    }
    // The realtime sessions opened on this server close (1001) and write
    // their threads' last turns, and its turns save what they had and write
    // their rows, before it returns: an upgraded connection and a turn's own
    // task are no part of the drain, and a process that exits now loses
    // them. In what is left of the bound.
    if let Some((stopped, at)) = stopped.get().copied() {
        let left = within.saturating_sub(at.elapsed());
        let open = state.stops.ended(stopped, left).await;
        if open > 0 {
            tracing::warn!(
                "{open} realtime session(s) or turn(s) had not ended {} s after the stop; the \
                 server returns without them",
                within.as_secs_f64()
            );
        }
    }
    Ok(())
}

/// `app` with `served_at` on every request, each counted in flight
/// ([`Stops::request`]): what both listeners serve.
pub(super) fn stamped(state: &SharedState, app: Router, served_at: ServedAt) -> Router {
    app.layer(axum::middleware::from_fn_with_state(
        (state.clone(), served_at),
        in_flight_mw,
    ))
    .layer(axum::Extension(served_at))
}

/// Counts every request in flight on this server, as it came in ("GET
/// /chat/api/feed"), until its response has been sent whole or dropped:
/// what a drain that runs out of time names ([`Stops::open_requests`]).
async fn in_flight_mw(
    axum::extract::State((state, served_at)): axum::extract::State<(SharedState, ServedAt)>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let what = format!("{} {}", req.method(), req.uri().path());
    let guard = state.stops.request(served_at, what);
    let res = next.run(req).await;
    // Held by the body, which goes when the response has been sent whole
    // (or the client went away).
    res.map(|body| {
        axum::body::Body::new(Held {
            body,
            _guard: guard,
        })
    })
}

/// A response body that holds its request's [`InFlight`] until it is sent
/// whole or dropped, and says its length as the body it wraps does: hyper
/// writes `content-length` from it, and a WebSocket client reads a refused
/// upgrade's body raw, as it came with the head, so a chunked one reads as
/// no JSON at all (device link WP3; `map_frame` dropped the size hint).
struct Held {
    body: axum::body::Body,
    _guard: InFlight,
}

impl hyper::body::Body for Held {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        hyper::body::Body::poll_frame(std::pin::Pin::new(&mut self.get_mut().body), cx)
    }

    fn is_end_stream(&self) -> bool {
        hyper::body::Body::is_end_stream(&self.body)
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        hyper::body::Body::size_hint(&self.body)
    }
}

/// The next Ctrl-C (SIGINT) or SIGTERM, by name: what a quit starts on, in
/// the headless binary and the tray app alike. A SIGTERM handler that cannot
/// be installed leaves Ctrl-C alone, and says so.
pub async fn quit_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => tokio::select! {
            _ = tokio::signal::ctrl_c() => "Ctrl-C",
            _ = term.recv() => "SIGTERM",
        },
        Err(e) => {
            tracing::warn!("SIGTERM handler not installed ({e}); Ctrl-C only");
            let _ = tokio::signal::ctrl_c().await;
            "Ctrl-C"
        }
    }
}

/// The exit status a process ends with when a second `signal`
/// ([`quit_signal`]'s name for it) makes it exit at once: 128 plus the
/// signal's number, as a shell reports a process the signal ended — 143 for
/// SIGTERM, 130 for Ctrl-C (SIGINT) (review P-14).
pub fn signal_exit_code(signal: &str) -> i32 {
    if signal == "SIGTERM" {
        128 + 15
    } else {
        128 + 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_signal_exits_with_its_own_status() {
        assert_eq!(signal_exit_code("SIGTERM"), 143);
        assert_eq!(signal_exit_code("Ctrl-C"), 130);
    }

    #[tokio::test]
    async fn a_stop_waits_for_its_own_server_s_sessions() {
        let stops = Stops::default();
        let old = stops.running();
        let stopped = stops.stop();
        let new = stops.running();
        assert_eq!(stops.ended(stopped, Duration::from_millis(20)).await, 1);
        drop(old);
        assert_eq!(
            stops.ended(stopped, Duration::from_secs(5)).await,
            0,
            "the next server's session is not waited for"
        );
        drop(new);
        assert_eq!(stops.ended(stops.stop(), Duration::ZERO).await, 0);
        // A row being written is waited for by any stop.
        let row = stops.writing();
        assert_eq!(
            stops.ended(stops.stop(), Duration::from_millis(20)).await,
            1
        );
        drop(row);
        assert_eq!(stops.ended(stops.now(), Duration::ZERO).await, 0);
    }

    /// Device link design §4.4: the device listener's stop ends its own
    /// streams and waits for its own sessions, never the plain listener's,
    /// and the other way round.
    #[tokio::test]
    async fn a_listener_s_stop_leaves_the_other_listener_alone() {
        let stops = Stops::default();
        let plain = stops.running();
        let device = stops.running_at(stops.now_of(Via::Devices));
        let mut plain_stream = until_stopped(
            &stops,
            Some(stops.now()),
            futures::stream::pending::<u8>(),
            Some(1),
        );
        let mut device_stream = until_stopped(
            &stops,
            Some(stops.now_of(Via::Devices)),
            futures::stream::pending::<u8>(),
            Some(2),
        );
        assert_eq!(
            (stops.running_on(Via::Plain), stops.running_on(Via::Devices)),
            (1, 1)
        );
        let stopped = stops.stop_of(Via::Devices);
        assert_eq!(
            device_stream.next().await,
            Some(2),
            "the device stream ends"
        );
        assert_eq!(device_stream.next().await, None);
        assert_eq!(
            stops.ended(stopped, Duration::from_millis(20)).await,
            1,
            "the device session is waited for"
        );
        drop(device);
        assert_eq!(
            stops.ended(stopped, Duration::from_millis(20)).await,
            0,
            "and the plain one is not"
        );
        let poll = futures::FutureExt::now_or_never(plain_stream.next());
        assert!(poll.is_none(), "the plain stream runs on");
        let stopped = stops.stop_of(Via::Plain);
        assert_eq!(plain_stream.next().await, Some(1));
        drop(plain);
        assert_eq!(stops.ended(stopped, Duration::ZERO).await, 0);
    }

    /// A counted request's answer keeps its length: hyper writes
    /// `content-length` from it, which a WebSocket client reading a refused
    /// upgrade's body needs (device link WP3), and the count ends with the
    /// body.
    #[tokio::test]
    async fn a_counted_answer_keeps_its_length() {
        use tower::ServiceExt;
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let at = state.stops.now();
        let app = stamped(
            &state,
            Router::new().route("/", axum::routing::get(|| async { "twelve bytes" })),
            at,
        );
        let resp = app
            .oneshot(axum::http::Request::new(axum::body::Body::empty()))
            .await
            .unwrap();
        let body = resp.into_body();
        assert_eq!(hyper::body::Body::size_hint(&body).exact(), Some(12));
        assert_eq!(state.stops.open_requests(at), ["GET /"]);
        drop(body);
        assert!(state.stops.open_requests(at).is_empty());
    }

    #[test]
    fn the_requests_in_flight_are_named_per_server() {
        let stops = Stops::default();
        let g0 = stops.now();
        let a = stops.request(g0, "GET /chat/api/feed".into());
        stops.stop();
        let g1 = stops.now();
        let _b = stops.request(g1, "GET /api/events".into());
        let d = stops.now_of(Via::Devices);
        let _c = stops.request(d, "GET /chat/api/feed".into());
        assert_eq!(stops.open_requests(g0), ["GET /chat/api/feed"]);
        drop(a);
        assert!(stops.open_requests(g0).is_empty());
        assert_eq!(stops.open_requests(g1), ["GET /api/events"]);
        assert_eq!(stops.open_requests(d), ["GET /chat/api/feed"]);
    }

    #[tokio::test]
    async fn a_stream_ends_at_its_server_s_stop_and_not_at_an_earlier_one() {
        let stops = Stops::default();
        let forever = futures::stream::pending::<u8>();
        let mut old = until_stopped(&stops, None, forever, Some(9));
        // A request that came in before the stop, its stream built after it
        // (review F-3): it ends with the server it came in on.
        let came_in = stops.now();
        stops.stop();
        let mut late = until_stopped(
            &stops,
            Some(came_in),
            futures::stream::pending::<u8>(),
            None,
        );
        assert_eq!(late.next().await, None, "ended at once");
        // Opened on the next server: the stop before it is not its own.
        let mut new = until_stopped(&stops, None, futures::stream::iter([1u8, 2]), None);
        assert_eq!(old.next().await, Some(9), "the last frame says it");
        assert_eq!(old.next().await, None);
        assert_eq!(new.next().await, Some(1));
        assert_eq!(new.next().await, Some(2));
        assert_eq!(new.next().await, None, "it ended by itself");
        let mut quiet = until_stopped(&stops, None, futures::stream::pending::<u8>(), None);
        stops.stop();
        assert_eq!(quiet.next().await, None, "no frame to say it in");
    }
}
