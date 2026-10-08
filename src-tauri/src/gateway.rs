//! The gateway this process serves, as the window must see it (chat-voice
//! §13.3, WP11 review m3).
//!
//! The window's navigation filter, the microphone permission and the command
//! gate all ask whether the window's document is at the gateway. The answer
//! is the origin of the port **this process holds** ([`Serving`]), never the
//! one the window happened to be built for:
//!
//! - **A failed bind** serves nothing. Whoever holds that port (the installed
//!   app beside a hand-made copy, a headless instance on the same dir) gets
//!   no window, no navigation, no microphone and no command. A debug build
//!   refuses to start; a release build says why in a dialog.
//! - **"Restart gateway"** after a `bind_addr` change serves on another
//!   origin. From the moment the old server stops, the old origin is nothing;
//!   once the new bind holds, the window is sent to a fresh login on the new
//!   origin (a new single-use nonce). Until it commits there, its old
//!   document is "not the gateway's origin".
//!
//! The window's host is loopback for a wildcard bind and the bound address
//! itself otherwise: a window on `127.0.0.1` beside a gateway bound to a LAN
//! address would reach whoever holds loopback on that port.
//!
//! **Quit stops the server first** (review F-1). Every exit — the tray's
//! Quit, an update's restart, SIGTERM or Ctrl-C — runs [`quit`]: the
//! server's stop (its streams end, every realtime session closes with
//! 1001), then the wait until it returned, which is once its sessions and
//! turns have ended (their last turns and rows written), at most
//! [`QUIT_WITHIN`] and said in the log when that runs out; only then the
//! model containers stop. A container stopped first failed the sessions and
//! turns still running on it mid-reply. A second SIGTERM or Ctrl-C exits at
//! once ([`on_signals`]).
//!
//! **An update's restart runs it first, then restarts** (review P-2):
//! Tauri ignores `prevent_exit` for a restart's exit code, so the exit
//! events cannot hold a restart for the sequence. The updater calls
//! [`quit_then`] with [`AfterQuit::Restart`], and the restart is asked for
//! once the sequence has run. An exit that goes through while a quit runs
//! anyway waits for it in `RunEvent::Exit` ([`Gateway::wait_quit`]) rather
//! than stopping the containers a second time beside it.
//!
//! **A quit is seen, and always ends** (review P-11): the window goes and
//! the tray says "quitting" the moment it starts (`quit_feedback`); a Quit
//! asked for again meanwhile is logged; and the exit or restart that ends
//! it runs from a guard, so a panic inside the sequence still ends the app.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use lmgw_core::state::SharedState;
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use crate::WindowOrigin;

/// The origin this process serves, or `None` while it serves nothing.
/// Shared by everything that judges the window's document.
#[derive(Clone, Default)]
pub(crate) struct Serving(Arc<RwLock<Option<WindowOrigin>>>);

impl Serving {
    pub(crate) fn origin(&self) -> Option<WindowOrigin> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, origin: Option<WindowOrigin>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = origin;
    }
}

/// The base URL the window and *Open in Browser* use for a gateway bound at
/// `addr`: loopback for a wildcard bind (`0.0.0.0`, and `[::]`, which is
/// dual-stack on Linux), else the bound address itself, on the bound port.
pub(crate) fn base_url(addr: SocketAddr) -> String {
    let host = match addr.ip() {
        ip if ip.is_unspecified() => "127.0.0.1".to_string(),
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    format!("http://{host}:{}", addr.port())
}

/// The window origin of a gateway bound at `addr`.
pub(crate) fn origin_of(addr: SocketAddr) -> WindowOrigin {
    WindowOrigin::of(
        &format!("{}/", base_url(addr))
            .parse::<tauri::Url>()
            .expect("an IP and a port make a URL"),
    )
}

/// What a restart does to the window.
#[derive(Debug, PartialEq)]
pub(crate) enum AfterRestart {
    /// The window is already on the served origin.
    Keep,
    /// No window (none was built while nothing was served): build it.
    Open,
    /// The served origin moved: send the window to a fresh login there.
    Reopen,
    /// Nothing is served: close the window and say why.
    NotServing(String),
}

/// The window's fate after a restart, given the origin it was opened for
/// (`None`: no window — a hidden one still counts) and the restart's bind.
pub(crate) fn after_restart(
    opened_for: Option<&WindowOrigin>,
    bound: &Result<SocketAddr, String>,
) -> AfterRestart {
    match (bound, opened_for) {
        (Err(why), _) => AfterRestart::NotServing(why.clone()),
        (Ok(_), None) => AfterRestart::Open,
        (Ok(addr), Some(o)) if *o != origin_of(*addr) => AfterRestart::Reopen,
        (Ok(_), Some(_)) => AfterRestart::Keep,
    }
}

/// "Restart gateway": stop, bind `bind_addr` again (re-read), then settle
/// the window on the main thread ([`after_restart`]). The old origin is
/// nobody's from the stop on.
pub(crate) fn restart(app: &tauri::AppHandle, state: SharedState) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let gateway: tauri::State<Gateway> = app.state();
        gateway.stop();
        let bound = gateway.start(state.clone(), crate::bind_addr(&state)).await;
        match &bound {
            Ok(addr) => tracing::info!("gateway restarted on {addr}"),
            Err(why) => tracing::error!("gateway restart: {why}"),
        }
        let action = after_restart(gateway.window_origin().as_ref(), &bound);
        let handle = app.clone();
        if let Err(e) = app.run_on_main_thread(move || apply_restart(&handle, action)) {
            tracing::error!("after the restart: {e}");
        }
    });
}

/// After "Restart gateway" (on the main thread): keep the window, build it,
/// send it to a fresh login on the origin the gateway moved to, or close it
/// when nothing is served ([`after_restart`]).
fn apply_restart(app: &tauri::AppHandle, action: AfterRestart) {
    let gateway: tauri::State<Gateway> = app.state();
    match action {
        AfterRestart::Keep => {}
        AfterRestart::Open => crate::show_main_window(app),
        AfterRestart::Reopen => {
            let (Some(window), Ok(addr)) = (app.get_webview_window("main"), gateway.status())
            else {
                return;
            };
            let state: tauri::State<SharedState> = app.state();
            gateway.set_window_origin(Some(origin_of(addr)));
            match crate::login_url(&state, &addr).parse::<tauri::Url>() {
                // Not the URL in the log: it carries a login nonce.
                Ok(url) => match window.navigate(url) {
                    Ok(()) => tracing::info!("the window follows the gateway to {addr}"),
                    Err(e) => tracing::error!("sending the window to {addr}: {e}"),
                },
                Err(e) => tracing::error!("the login URL for {addr}: {e}"),
            }
        }
        AfterRestart::NotServing(why) => {
            if let Some(window) = app.get_webview_window("main") {
                if let Err(e) = window.destroy() {
                    tracing::error!("closing the window: {e}");
                }
            }
            gateway.set_window_origin(None);
            not_serving(app, &why);
        }
    }
}

/// Say that this process serves nothing, and why. No window is built then:
/// whoever holds the port must not get the window, its microphone or its
/// commands (chat-voice WP11 review m3).
pub(crate) fn not_serving(app: &tauri::AppHandle, why: &str) {
    tracing::error!("no window: the gateway is not serving: {why}");
    app.dialog()
        .message(format!(
            "lmgw is not serving, so there is no dashboard to open.\n\n{why}\n\nFree the \
             port (or stop whatever else holds it), then choose Restart gateway in the tray."
        ))
        .title("lmgw")
        .kind(MessageDialogKind::Error)
        .show(|_| {});
}

/// How long a quit waits for the server to return: its own bound
/// (`server::STOP_WITHIN`, which it never passes) and a margin for its task
/// to hand back. Said in the log when it runs out.
pub(crate) const QUIT_WITHIN: Duration =
    lmgw_core::server::STOP_WITHIN.saturating_add(Duration::from_secs(2));

/// How long an exit that went through while a quit runs waits for it
/// (`RunEvent::Exit`, review P-2): the server's bound, the containers'
/// (`lifecycle::SHUTDOWN_TIMEOUT`) and a margin for the benchmark's abort,
/// which runs before them. Said in the log when it runs out.
pub(crate) const EXIT_WAITS_WITHIN: Duration = QUIT_WITHIN
    .saturating_add(lmgw_core::runtime::lifecycle::SHUTDOWN_TIMEOUT)
    .saturating_add(Duration::from_secs(5));

/// The quit sequence (module doc): stop the server, wait at most
/// [`QUIT_WITHIN`] for it to return, then `containers` — the model
/// containers' stop (`lifecycle::shutdown`), which no session or turn of
/// this server still runs on then.
pub(crate) async fn quit<F: std::future::Future<Output = ()>>(gateway: &Gateway, containers: F) {
    if gateway.stop_and_wait(QUIT_WITHIN).await {
        tracing::info!("quit: the gateway has stopped; the model containers stop now");
    } else {
        tracing::info!("quit: the model containers stop now, without the gateway's return");
    }
    containers.await;
}

/// What ends the quit sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterQuit {
    /// The exit, with this code.
    Exit(i32),
    /// The restart into the installed version (an update).
    Restart,
}

impl AfterQuit {
    /// What an exit request with `code` asked for.
    fn of(code: Option<i32>) -> Self {
        match code {
            Some(tauri::RESTART_EXIT_CODE) => Self::Restart,
            Some(code) => Self::Exit(code),
            None => Self::Exit(0),
        }
    }
}

/// The quit sequence as one run: [`quit`], then `then` with whether the
/// sequence completed. `then` — the exit or the restart — runs from a guard,
/// and the quit is marked as ended before it, so a panic inside the
/// sequence still ends the app (review P-11): marked failed, and the exit
/// then stops the containers itself (`RunEvent::Exit`).
pub(crate) async fn run_quit<F, T>(gateway: &Gateway, containers: F, then: T)
where
    F: std::future::Future<Output = ()>,
    T: FnOnce(bool),
{
    let mut ended = QuitEnded {
        gateway,
        then: Some(then),
        completed: false,
    };
    quit(gateway, containers).await;
    ended.completed = true;
}

/// Ends a quit sequence when it drops ([`run_quit`]).
struct QuitEnded<'a, T: FnOnce(bool)> {
    gateway: &'a Gateway,
    then: Option<T>,
    completed: bool,
}

impl<T: FnOnce(bool)> Drop for QuitEnded<'_, T> {
    fn drop(&mut self) {
        if !self.completed {
            tracing::error!(
                "quit: the sequence failed inside lmgw; the app ends anyway, and its exit stops \
                 the model containers"
            );
        }
        self.gateway.quit_ended(self.completed);
        if let Some(then) = self.then.take() {
            then(self.completed);
        }
    }
}

/// What an exit request does now ([`Gateway::begin_quit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuitStep {
    /// The first: run the quit sequence, then exit.
    Start,
    /// The sequence runs: the exit waits for it.
    Running,
    /// The sequence has run (or ended in a failure): exit.
    Done,
}

/// Run the quit sequence, then `then` (module doc); where it stands
/// ([`QuitStep`]). The first request starts it, with the window and the
/// tray saying so; one that comes while it runs is logged, and waits for
/// it; once it has run, nothing starts.
pub(crate) fn quit_then(app: &tauri::AppHandle, then: AfterQuit) -> QuitStep {
    let gateway: tauri::State<Gateway> = app.state();
    let step = gateway.begin_quit();
    match step {
        QuitStep::Start => {
            crate::quit_feedback(app);
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let gateway: tauri::State<Gateway> = app.state();
                let state: tauri::State<SharedState> = app.state();
                let ends = app.clone();
                run_quit(
                    &gateway,
                    lmgw_core::runtime::lifecycle::shutdown(&state),
                    move |_| match then {
                        AfterQuit::Restart => ends.request_restart(),
                        AfterQuit::Exit(code) => ends.exit(code),
                    },
                )
                .await;
            });
        }
        QuitStep::Running => tracing::info!(
            "quit: asked again while the quit runs; it goes on and the app ends with it (a \
             second SIGTERM or Ctrl-C exits at once)"
        ),
        QuitStep::Done => {}
    }
    step
}

/// An exit was requested (`RunEvent::ExitRequested`): `true` when it must
/// wait — the first starts the quit sequence ([`quit_then`]) and asks for
/// the exit again once it has run, with the same code; one that comes while
/// it runs waits for it. A restart's exit is not held by this (module doc):
/// the updater runs the sequence first.
pub(crate) fn exit_requested(app: &tauri::AppHandle, code: Option<i32>) -> bool {
    quit_then(app, AfterQuit::of(code)) != QuitStep::Done
}

/// SIGTERM or Ctrl-C (SIGINT) quits as the tray's Quit does; a second one
/// exits at once, said in the log, without waiting for the stop or the model
/// containers (the next start adopts them), with the signal's exit status
/// (`server::signal_exit_code`: 143 for SIGTERM, 130 for Ctrl-C).
pub(crate) fn on_signals(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let first = lmgw_core::server::quit_signal().await;
        tracing::info!("{first}: quitting, as the tray's Quit does (again to exit at once)");
        app.exit(0);
        let second = lmgw_core::server::quit_signal().await;
        tracing::warn!(
            "{second} again: exiting now, without waiting for the stop to finish or the model \
             containers to stop (the next start adopts them)"
        );
        std::process::exit(lmgw_core::server::signal_exit_code(second));
    });
}

#[derive(Default)]
struct Inner {
    /// Bumped by every start and stop, so a server task that ends late does
    /// not clear a newer server's state.
    generation: u64,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    /// Fires when the current server's listener has closed.
    released: Option<tokio::sync::oneshot::Receiver<()>>,
    /// Fires when the current server's `serve` has returned: its sessions
    /// and turns have ended, or its bound ran out.
    served: Option<tokio::sync::oneshot::Receiver<()>>,
    /// Where this process serves, or why it does not.
    status: Option<Result<SocketAddr, String>>,
    /// The origin the window was opened (or last sent) for.
    window: Option<WindowOrigin>,
}

/// Where the quit sequence stands (`Gateway::quit`).
const QUIT_IDLE: u8 = 0;
const QUIT_RUNNING: u8 = 1;
const QUIT_DONE: u8 = 2;
/// It ended without completing (a panic inside it): the exit stops the
/// containers itself.
const QUIT_FAILED: u8 = 3;

/// The in-process gateway server: started by `setup`, bounced by "Restart
/// gateway".
#[derive(Default)]
pub(crate) struct Gateway {
    serving: Serving,
    inner: Arc<Mutex<Inner>>,
    /// Where the quit sequence stands: `QUIT_IDLE`, `QUIT_RUNNING`,
    /// `QUIT_DONE` or `QUIT_FAILED`.
    quit: AtomicU8,
    /// Wakes the waits for a running quit ([`Self::wait_quit`]).
    quit_ended: tokio::sync::Notify,
}

impl Gateway {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn serving(&self) -> Serving {
        self.serving.clone()
    }

    /// Where this process serves, or why it serves nothing.
    pub(crate) fn status(&self) -> Result<SocketAddr, String> {
        self.lock()
            .status
            .clone()
            .unwrap_or_else(|| Err("the gateway has not started".into()))
    }

    pub(crate) fn window_origin(&self) -> Option<WindowOrigin> {
        self.lock().window.clone()
    }

    pub(crate) fn set_window_origin(&self, origin: Option<WindowOrigin>) {
        self.lock().window = origin;
    }

    /// Bind `addr` and serve on it, after the previous server's listener has
    /// closed. `Ok` is the address this process now holds.
    pub(crate) async fn start(
        &self,
        state: SharedState,
        addr: SocketAddr,
    ) -> Result<SocketAddr, String> {
        let released = self.lock().released.take();
        if let Some(released) = released {
            // A dropped sender is a closed listener too (`serve` returned).
            let _ = released.await;
        }
        let listener = lmgw_core::server::bind(&state, addr)
            .await
            .and_then(|l| Ok((l.local_addr()?, l)));
        let mut inner = self.lock();
        inner.generation += 1;
        let (bound, listener) = match listener {
            Ok(ok) => ok,
            Err(e) => {
                let why = format!("{e:#}");
                inner.status = Some(Err(why.clone()));
                self.serving.set(None);
                return Err(why);
            }
        };
        let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();
        let (released, on_release) = tokio::sync::oneshot::channel();
        let (served, on_served) = tokio::sync::oneshot::channel::<()>();
        inner.shutdown = Some(shutdown);
        inner.released = Some(on_release);
        inner.served = Some(on_served);
        inner.status = Some(Ok(bound));
        self.serving.set(Some(origin_of(bound)));
        let generation = inner.generation;
        drop(inner);
        let (shared, serving) = (self.inner.clone(), self.serving.clone());
        tauri::async_runtime::spawn(async move {
            // Dropped as this task ends: `serve` has returned.
            let _served = served;
            let ended = lmgw_core::server::serve(
                state,
                listener,
                async {
                    let _ = stopped.await;
                },
                Some(released),
            )
            .await;
            let why = match ended {
                Ok(()) => "the gateway stopped".to_string(),
                Err(e) => {
                    tracing::error!("gateway server exited with error: {e:#}");
                    format!("the gateway stopped: {e:#}")
                }
            };
            let mut inner = shared.lock().unwrap_or_else(|e| e.into_inner());
            if inner.generation == generation {
                inner.status = Some(Err(why));
                serving.set(None);
            }
        });
        Ok(bound)
    }

    /// Stop serving: the origin is nobody's from here on.
    pub(crate) fn stop(&self) {
        let mut inner = self.lock();
        inner.generation += 1;
        inner.status = Some(Err("the gateway is restarting".into()));
        self.serving.set(None);
        if let Some(tx) = inner.shutdown.take() {
            let _ = tx.send(());
        }
    }

    /// [`Self::stop`], then wait at most `within` for the server to return
    /// (`true`), saying so in the log when the bound runs out (`false`). A
    /// gateway that serves nothing has nothing to wait for.
    pub(crate) async fn stop_and_wait(&self, within: Duration) -> bool {
        let served = self.lock().served.take();
        self.stop();
        let Some(served) = served else {
            return true;
        };
        // A dropped sender is the task's end, as a sent value would be.
        match tokio::time::timeout(within, served).await {
            Ok(_) => true,
            Err(_) => {
                tracing::warn!(
                    "the gateway had not stopped {} s after the quit; the quit goes on without \
                     it",
                    within.as_secs_f64()
                );
                false
            }
        }
    }

    /// An exit was requested: where the quit sequence stands, and the first
    /// request starts it ([`exit_requested`]).
    pub(crate) fn begin_quit(&self) -> QuitStep {
        match self.quit.compare_exchange(
            QUIT_IDLE,
            QUIT_RUNNING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => QuitStep::Start,
            Err(QUIT_RUNNING) => QuitStep::Running,
            Err(_) => QuitStep::Done,
        }
    }

    /// The quit sequence ended — `completed`, its containers' stop with it,
    /// or not (a panic inside it): the next exit request goes through, and
    /// a wait for it ends.
    pub(crate) fn quit_ended(&self, completed: bool) {
        let to = if completed { QUIT_DONE } else { QUIT_FAILED };
        self.quit.store(to, Ordering::Release);
        self.quit_ended.notify_waiters();
    }

    /// Whether the quit sequence has run (its containers' stop with it).
    pub(crate) fn quit_done(&self) -> bool {
        self.quit.load(Ordering::Acquire) == QUIT_DONE
    }

    /// Whether a quit sequence runs now.
    pub(crate) fn quitting(&self) -> bool {
        self.quit.load(Ordering::Acquire) == QUIT_RUNNING
    }

    /// Wait at most `within` for a quit that runs now to end; `true` when
    /// none runs (any more). Said in the log when the bound runs out.
    pub(crate) async fn wait_quit(&self, within: Duration) -> bool {
        let ended = self.quit_ended.notified();
        tokio::pin!(ended);
        // Registered before the state is read: an end in between wakes it.
        ended.as_mut().enable();
        if !self.quitting() {
            return true;
        }
        if tokio::time::timeout(within, ended).await.is_ok() {
            return true;
        }
        tracing::warn!(
            "the exit waited {} s for the quit that runs and goes on without it",
            within.as_secs_f64()
        );
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_window_goes_where_this_process_is_bound() {
        assert_eq!(base_url(addr("127.0.0.1:8899")), "http://127.0.0.1:8899");
        assert_eq!(base_url(addr("0.0.0.0:8001")), "http://127.0.0.1:8001");
        assert_eq!(base_url(addr("[::]:8001")), "http://127.0.0.1:8001");
        assert_eq!(base_url(addr("[::1]:8001")), "http://[::1]:8001");
        assert_eq!(base_url(addr("127.0.0.2:9")), "http://127.0.0.2:9");
        // Bound to one LAN address: loopback on that port is somebody else.
        assert_eq!(base_url(addr("192.0.2.7:8001")), "http://192.0.2.7:8001");
        let o = origin_of(addr("0.0.0.0:8001"));
        assert_eq!((o.host.as_str(), o.port), ("127.0.0.1", Some(8001)));
        assert_eq!(origin_of(addr("[::1]:8001")).host, "[::1]");
    }

    #[test]
    fn a_restart_reopens_the_window_only_when_the_origin_moved() {
        let old = origin_of(addr("127.0.0.1:8001"));
        let same: Result<SocketAddr, String> = Ok(addr("127.0.0.1:8001"));
        let moved: Result<SocketAddr, String> = Ok(addr("127.0.0.1:8002"));
        let lan: Result<SocketAddr, String> = Ok(addr("192.0.2.7:8001"));
        let failed: Result<SocketAddr, String> = Err("binding 127.0.0.1:8002: in use".into());

        assert_eq!(after_restart(Some(&old), &same), AfterRestart::Keep);
        assert_eq!(after_restart(Some(&old), &moved), AfterRestart::Reopen);
        assert_eq!(
            after_restart(Some(&old), &lan),
            AfterRestart::Reopen,
            "the same port on another address is another origin"
        );
        assert_eq!(
            after_restart(None, &moved),
            AfterRestart::Open,
            "no window: the last start or restart served nothing"
        );
        for window in [Some(&old), None] {
            assert_eq!(
                after_restart(window, &failed),
                AfterRestart::NotServing("binding 127.0.0.1:8002: in use".into())
            );
        }
    }

    /// Review F-1: an exit request starts the quit sequence once; one that
    /// comes while it runs waits; once it has run, the exit goes through.
    #[test]
    fn an_exit_runs_the_quit_sequence_once() {
        let g = Gateway::default();
        assert!(!g.quit_done());
        assert_eq!(g.begin_quit(), QuitStep::Start);
        assert_eq!(g.begin_quit(), QuitStep::Running, "a second Quit waits");
        assert!(g.quitting());
        g.quit_ended(true);
        assert!(g.quit_done() && !g.quitting());
        assert_eq!(g.begin_quit(), QuitStep::Done, "the exit goes through");
        assert_eq!(
            AfterQuit::of(Some(tauri::RESTART_EXIT_CODE)),
            AfterQuit::Restart
        );
        assert_eq!(AfterQuit::of(None), AfterQuit::Exit(0));
    }

    /// Review P-2: an exit that went through while a quit runs waits for
    /// it, and does not start a second one; a bound that runs out is said.
    #[tokio::test]
    async fn an_exit_waits_for_the_quit_that_runs() {
        let g = Arc::new(Gateway::default());
        assert!(g.wait_quit(Duration::ZERO).await, "none runs");
        assert_eq!(g.begin_quit(), QuitStep::Start);
        assert!(!g.wait_quit(Duration::from_millis(20)).await, "the bound");
        let ends = g.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            ends.quit_ended(true);
        });
        let started = std::time::Instant::now();
        assert!(g.wait_quit(Duration::from_secs(10)).await);
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(g.quit_done());
    }

    /// Review P-11: a panic inside the sequence still ends the app — the
    /// quit is marked as ended (failed, so the exit stops the containers)
    /// and the exit runs.
    #[tokio::test]
    async fn a_panic_inside_the_quit_still_ends_the_app() {
        let g = Arc::new(Gateway::default());
        let ended = Arc::new(Mutex::new(None));
        assert_eq!(g.begin_quit(), QuitStep::Start);
        let (gw, at_end) = (g.clone(), ended.clone());
        let run = tokio::spawn(async move {
            run_quit(
                &gw,
                async { panic!("a containers' stop that panics") },
                |completed| *at_end.lock().unwrap() = Some(completed),
            )
            .await;
        });
        assert!(run.await.unwrap_err().is_panic());
        assert_eq!(*ended.lock().unwrap(), Some(false), "the exit ran");
        assert!(
            !g.quitting() && !g.quit_done(),
            "failed: the exit stops them"
        );
        assert_eq!(g.begin_quit(), QuitStep::Done, "and is not held");
    }

    /// One request to the gateway at `addr` with `key`, over a raw socket:
    /// the reader of its response.
    async fn get(addr: SocketAddr, path: &str, key: &str) -> tokio::net::TcpStream {
        use tokio::io::AsyncWriteExt;
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {key}\r\n\
             Accept: text/event-stream\r\n\r\n"
        );
        s.write_all(req.as_bytes()).await.unwrap();
        s
    }

    /// Read `s` until `what` came, or to its end (`what` empty); all read.
    async fn read_until(s: &mut tokio::net::TcpStream, what: &str) -> String {
        use tokio::io::AsyncReadExt;
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = s.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
            if !what.is_empty() && String::from_utf8_lossy(&got).contains(what) {
                break;
            }
        }
        String::from_utf8_lossy(&got).into_owned()
    }

    /// Review F-1: the shell's quit stops the server and waits for it before
    /// the model containers' stop runs — with a Chat feed open (it ends, its
    /// chunked body closed) and a session still running past the stop (the
    /// containers' stop finds none running).
    #[tokio::test]
    async fn a_quit_stops_the_server_and_waits_before_the_containers_stop() {
        let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
        let key = state
            .snapshot()
            .owner_key(lmgw_core::agents::token::OWNER_DASHBOARD)
            .expect("the dashboard's key")
            .to_string();
        let g = Gateway::default();
        let addr = g
            .start(state.clone(), addr("127.0.0.1:0"))
            .await
            .expect("serving");
        let mut feed = get(addr, "/chat/api/feed", &key).await;
        let hello = read_until(&mut feed, "event: hello").await;
        assert!(hello.starts_with("HTTP/1.1 200"), "{hello}");
        // A session that takes a moment to end after the stop, as a bound
        // session writing its last turns does.
        let session = state.stops.running();
        let stops = state.clone();
        tokio::spawn(async move {
            stops.stops.stopped_after(0).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(session);
        });
        let seen = Arc::new(Mutex::new(None));
        let at_stop = seen.clone();
        let st = state.clone();
        let started = std::time::Instant::now();
        quit(&g, async move {
            let running = st.stops.ended(u64::MAX, Duration::ZERO).await;
            *at_stop.lock().unwrap() = Some(running);
        })
        .await;
        assert_eq!(
            *seen.lock().unwrap(),
            Some(0),
            "the containers' stop ran once nothing of the server still ran"
        );
        assert!(started.elapsed() >= Duration::from_millis(300), "it waited");
        assert!(started.elapsed() < QUIT_WITHIN, "and not for the bound");
        let rest = read_until(&mut feed, "").await;
        assert!(
            rest.ends_with("0\r\n\r\n"),
            "the feed ended whole: {rest:?}"
        );
        assert!(g.serving().origin().is_none());
        assert!(
            g.stop_and_wait(Duration::ZERO).await,
            "a second stop has nothing to wait for"
        );
    }

    /// Review P-2: an update's restart runs the same sequence first — the
    /// server stops and is waited for, with a feed open and a session still
    /// ending, then the containers stop — and only then asks for the
    /// restart, which finds the quit done: its exit is not held, and
    /// `RunEvent::Exit` stops nothing a second time.
    #[tokio::test]
    async fn an_update_s_restart_runs_the_quit_sequence_before_it_restarts() {
        let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
        let key = state
            .snapshot()
            .owner_key(lmgw_core::agents::token::OWNER_DASHBOARD)
            .expect("the dashboard's key")
            .to_string();
        let g = Gateway::default();
        let addr = g
            .start(state.clone(), addr("127.0.0.1:0"))
            .await
            .expect("serving");
        let mut feed = get(addr, "/chat/api/feed", &key).await;
        let hello = read_until(&mut feed, "event: hello").await;
        assert!(hello.starts_with("HTTP/1.1 200"), "{hello}");
        let session = state.stops.running();
        let stops = state.clone();
        tokio::spawn(async move {
            stops.stops.stopped_after(0).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(session);
        });
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let (at_stop, at_restart) = (order.clone(), order.clone());
        let st = state.clone();
        let started = std::time::Instant::now();
        // As `quit_then` runs it for the updater's `AfterQuit::Restart`.
        assert_eq!(g.begin_quit(), QuitStep::Start);
        run_quit(
            &g,
            async move {
                let running = st.stops.ended(u64::MAX, Duration::ZERO).await;
                at_stop
                    .lock()
                    .unwrap()
                    .push(format!("containers stop, {running} running"));
            },
            |completed| {
                at_restart.lock().unwrap().push(format!(
                    "restart, completed {completed}, quit done {}",
                    g.quit_done()
                ))
            },
        )
        .await;
        assert_eq!(
            *order.lock().unwrap(),
            [
                "containers stop, 0 running",
                "restart, completed true, quit done true"
            ],
            "the server, then the containers, then the restart"
        );
        assert!(started.elapsed() >= Duration::from_millis(300), "it waited");
        let rest = read_until(&mut feed, "").await;
        assert!(
            rest.ends_with("0\r\n\r\n"),
            "the feed ended whole: {rest:?}"
        );
        assert_eq!(
            g.begin_quit(),
            QuitStep::Done,
            "the restart's exit request goes through"
        );
        assert!(
            g.wait_quit(Duration::ZERO).await,
            "and its exit waits for nothing"
        );
    }

    #[test]
    fn serving_is_cleared_by_a_stop() {
        let g = Gateway::default();
        assert!(g.serving().origin().is_none());
        assert!(g.status().is_err(), "nothing started yet");
        g.serving.set(Some(origin_of(addr("127.0.0.1:8001"))));
        g.lock().status = Some(Ok(addr("127.0.0.1:8001")));
        let shared = g.serving();
        g.stop();
        assert!(shared.origin().is_none(), "every holder sees the stop");
        assert!(g.status().is_err());
    }
}
