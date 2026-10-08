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

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use futures::stream::{BoxStream, StreamExt};
use futures::Stream;
use tokio::sync::watch;

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

/// The generation of the server a request came in on: put on every request
/// by `serve_app`, before anything else reads it (review F-3). A stream or a
/// session it opens ends at that server's stop, however long it took to get
/// going. Read through `RequestCtx::served_at`, or from the extensions where
/// no context is resolved (the agent proxy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServedAt(pub u64);

/// The servers' stops: a generation that moves on each time one stops, the
/// sessions and turns running per generation they were opened at, and the
/// requests in flight.
#[derive(Debug)]
pub struct Stops {
    generation: watch::Sender<u64>,
    running: Arc<watch::Sender<BTreeMap<u64, usize>>>,
    open: Arc<std::sync::Mutex<Open>>,
}

/// The requests in flight, by a number of their own: what a drain that ran
/// out of time names.
#[derive(Debug, Default)]
struct Open {
    next: u64,
    by_id: BTreeMap<u64, (u64, String)>,
}

impl Default for Stops {
    fn default() -> Self {
        Self {
            generation: watch::Sender::new(0),
            running: Arc::new(watch::Sender::new(BTreeMap::new())),
            open: Arc::default(),
        }
    }
}

/// A session or a turn running, from its start to its end
/// ([`Stops::running_at`]).
#[derive(Debug)]
pub struct Running {
    at: u64,
    running: Arc<watch::Sender<BTreeMap<u64, usize>>>,
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
    /// The generation now: a stream opened now ends at the next stop.
    pub fn now(&self) -> u64 {
        *self.generation.borrow()
    }

    /// A server stopped: every stream opened before this ends. The
    /// generation it stopped: what [`Self::ended`] waits on.
    pub fn stop(&self) -> u64 {
        let mut stopped = 0;
        self.generation.send_modify(|g| {
            stopped = *g;
            *g += 1;
        });
        stopped
    }

    /// A session or a turn starts now: held until it has ended.
    pub fn running(&self) -> Running {
        self.running_at(self.now())
    }

    /// A session or a turn of a request that came in on the server of
    /// generation `at` ([`ServedAt`]): held until it has ended. That
    /// server's stop waits for it.
    pub fn running_at(&self, at: u64) -> Running {
        self.running.send_modify(|m| *m.entry(at).or_default() += 1);
        Running {
            at,
            running: self.running.clone(),
        }
    }

    /// Work every stop waits for, whichever server it belongs to: a request
    /// row being written. Counted at the first generation, which every stop
    /// takes in.
    pub fn writing(&self) -> Running {
        self.running_at(0)
    }

    /// `at` when a request carried it, else the generation now: what a
    /// stream or a turn without a request's generation is opened at.
    pub fn at_or_now(&self, at: Option<u64>) -> u64 {
        at.unwrap_or_else(|| self.now())
    }

    /// Wait, at most `within`, until every session and turn opened at
    /// generation `stopped` or before has ended; how many still run.
    pub async fn ended(&self, stopped: u64, within: Duration) -> usize {
        let mut rx = self.running.subscribe();
        let open = |m: &BTreeMap<u64, usize>| m.range(..=stopped).map(|(_, n)| n).sum::<usize>();
        let _ = tokio::time::timeout(within, rx.wait_for(|m| open(m) == 0)).await;
        let n = open(&rx.borrow());
        n
    }

    /// A request came in on the server of generation `at`, as `what`
    /// ("GET /chat/api/feed"): in flight until the guard drops.
    pub fn request(&self, at: u64, what: String) -> InFlight {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        open.next += 1;
        let id = open.next;
        open.by_id.insert(id, (at, what));
        InFlight {
            id,
            open: self.open.clone(),
        }
    }

    /// The requests of the server of generation `at` still in flight, as
    /// they came in ("GET /chat/api/feed"), oldest first.
    pub fn open_requests(&self, at: u64) -> Vec<String> {
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_id
            .values()
            .filter(|(g, _)| *g == at)
            .map(|(_, what)| what.clone())
            .collect()
    }

    /// Resolves once a stop came after generation `opened_at`.
    pub fn stopped_after(&self, opened_at: u64) -> impl Future<Output = ()> + Send + 'static {
        let mut rx = self.generation.subscribe();
        async move {
            // The sender lives in the state for as long as any stream does;
            // a closed channel is a state that went, and nothing waits on it.
            let _ = rx.wait_for(|g| *g > opened_at).await;
        }
    }
}

/// `events` until the server of generation `at` stops (`None`: the server
/// serving now — a stream with no request's [`ServedAt`]), then `last` (the
/// frame that says so, if the stream has one) and the end. Dropping the
/// stream drops what produced `events`, as a client that goes away does.
pub fn until_stopped<T, S>(
    stops: &Stops,
    at: Option<u64>,
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
/// request carrying this server's generation and counted in flight; at
/// `shutdown`, the stop (module doc), all of it within `within`.
pub(super) async fn serve_stopping<L>(
    state: &SharedState,
    listener: L,
    app: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    within: std::time::Duration,
) -> anyhow::Result<()>
where
    L: axum::serve::Listener<Addr = std::net::SocketAddr>,
    for<'a> std::net::SocketAddr:
        axum::extract::connect_info::Connected<axum::serve::IncomingStream<'a, L>>,
{
    // This server's generation, put on every request before anything reads
    // it (review F-3): a stream or a session ends at this server's stop
    // however long it took to open, and the stop waits for the turns its
    // requests started. Outermost, so even the agent proxy (above the
    // principal) sees it, and every request is counted in flight.
    let served_at = state.stops.now();
    let app = app
        .layer(axum::middleware::from_fn_with_state(
            (state.clone(), served_at),
            in_flight_mw,
        ))
        .layer(axum::Extension(ServedAt(served_at)));
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
        let _ = at.set((st.stops.stop(), tokio::time::Instant::now()));
    };
    // With the peer address on every request: the agent proxy tells an app
    // who reached lmgw in `X-Forwarded-For` (origins §4.6), and that address
    // is the TCP peer's, never a header a client wrote.
    let drain = std::future::IntoFuture::into_future(
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
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

/// Counts every request in flight on this server, as it came in ("GET
/// /chat/api/feed"), until its response has been sent whole or dropped:
/// what a drain that runs out of time names ([`Stops::open_requests`]).
async fn in_flight_mw(
    axum::extract::State((state, served_at)): axum::extract::State<(SharedState, u64)>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use http_body_util::BodyExt;
    let what = format!("{} {}", req.method(), req.uri().path());
    let guard = state.stops.request(served_at, what);
    let res = next.run(req).await;
    // Held by the body, which goes when the response has been sent whole
    // (or the client went away); size hints and trailers kept.
    res.map(|body| {
        axum::body::Body::new(body.map_frame(move |frame| {
            let _held = &guard;
            frame
        }))
    })
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

    #[test]
    fn the_requests_in_flight_are_named_per_server() {
        let stops = Stops::default();
        let a = stops.request(0, "GET /chat/api/feed".into());
        let _b = stops.request(1, "GET /api/events".into());
        assert_eq!(stops.open_requests(0), ["GET /chat/api/feed"]);
        drop(a);
        assert!(stops.open_requests(0).is_empty());
        assert_eq!(stops.open_requests(1), ["GET /api/events"]);
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
