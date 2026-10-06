//! The main window appears once its page has drawn the app. Shown at once,
//! it was an empty pane for as long as the WASM took to load and start: about
//! 0.4 s for a release build, 20 to 30 s for a debug one (WebKitGTK 2.54,
//! measured 2026-10-06).
//!
//! desktop.js passes Trunk's `TrunkApplicationStarted` on as the event
//! [`MOUNTED`]. If it never comes (the gateway answered with an error page,
//! the WASM failed to start), the window shows anyway after [`FALLBACK`], with
//! a warning in the log, so it can never stay hidden.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{Listener, Manager, WebviewWindow};

/// What desktop.js emits once the app has mounted.
const MOUNTED: &str = "lmgw:mounted";

/// How long a hidden window waits for the page: over ten times what a release
/// build needs. A debug build's WASM is some 240 MB and takes longer; its
/// window shows at the fallback, before the app is drawn, as every window
/// did before.
const FALLBACK: Duration = Duration::from_secs(5);

/// Shows and focuses the (hidden) window when its app has mounted, or after
/// the fallback, whichever comes first.
pub(crate) fn when_mounted(window: &WebviewWindow) {
    let started = Instant::now();
    let shown = Arc::new(AtomicBool::new(false));
    let show = {
        let window = window.clone();
        // true when this call showed it
        move || {
            if shown.swap(true, Ordering::SeqCst) {
                return false;
            }
            let _ = window.show();
            let _ = window.set_focus();
            true
        }
    };
    let on_mount = show.clone();
    window.app_handle().once(MOUNTED, move |_| {
        tracing::info!("the app mounted after {} ms", started.elapsed().as_millis());
        let _ = on_mount();
    });
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FALLBACK).await;
        if show() {
            tracing::warn!(
                "window shown before the app: it did not report its mount within {} s",
                FALLBACK.as_secs()
            );
        }
    });
}
