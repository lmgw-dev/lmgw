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

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};

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

#[derive(Default)]
struct Inner {
    /// Bumped by every start and stop, so a server task that ends late does
    /// not clear a newer server's state.
    generation: u64,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    /// Fires when the current server's listener has closed.
    released: Option<tokio::sync::oneshot::Receiver<()>>,
    /// Where this process serves, or why it does not.
    status: Option<Result<SocketAddr, String>>,
    /// The origin the window was opened (or last sent) for.
    window: Option<WindowOrigin>,
}

/// The in-process gateway server: started by `setup`, bounced by "Restart
/// gateway".
#[derive(Default)]
pub(crate) struct Gateway {
    serving: Serving,
    inner: Arc<Mutex<Inner>>,
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
        inner.shutdown = Some(shutdown);
        inner.released = Some(on_release);
        inner.status = Some(Ok(bound));
        self.serving.set(Some(origin_of(bound)));
        let generation = inner.generation;
        drop(inner);
        let (shared, serving) = (self.inner.clone(), self.serving.clone());
        tauri::async_runtime::spawn(async move {
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
