//! The main window's size: the first one, and the one it is given back.
//!
//! The window keeps the size the owner left it at, and whether it was
//! maximized, across launches: `window.json` in the instance's own data dir
//! (a dev instance's scratch dir is its own, so it never touches the
//! installed app's). Not its position: on Wayland the compositor places
//! windows, a client cannot.
//!
//! With nothing saved, the default is sized for the interface scale the app
//! starts at (125 %, `ui_scale`), where one CSS pixel is 1.25 window pixels:
//! the sidebar's whole nav (every group and the foot) measured 819 CSS px
//! tall on 2026-10-19 with Labs and Models both unfolded (669 folded), so
//! 1024 px of window. 1100 leaves about 60 CSS px for two more nav items
//! before the unfolded nav scrolls; the width keeps the old 3:2 shape so the
//! pages grow with it.
//!
//! Saved or default, a window never opens larger than its monitor's work
//! area (the monitor may have changed since): on a smaller screen it takes
//! what there is, and the nav scrolls there.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{Manager, Monitor, WebviewWindow, WindowEvent};

/// Logical px, what `inner_size` takes.
pub const DEFAULT: (f64, f64) = (1650.0, 1100.0);

/// What the window's frame (the title bar KWin draws) needs beyond the inner
/// size, and a panel the work area may not count: GTK on Wayland reports the
/// whole output as the work area.
const FRAME: f64 = 64.0;

/// How long a resize has to rest before it is written: a drag sends a
/// stream of sizes, and only where it ends matters.
const SETTLE: Duration = Duration::from_millis(500);

/// The window label this follows.
const LABEL: &str = "main";

/// What `window.json` holds. The size is the unmaximized one, so a window
/// restored maximized un-maximizes to it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub width: f64,
    pub height: f64,
    pub maximized: bool,
}

impl Saved {
    fn valid(&self) -> bool {
        [self.width, self.height]
            .iter()
            .all(|v| v.is_finite() && *v > 0.0)
    }
}

/// The file in data dir `dir`.
pub fn file(dir: &Path) -> PathBuf {
    dir.join("window.json")
}

/// The saved state at `path`: `None` when there is none yet, and when it is
/// unreadable or invalid (said in the log; the default applies then).
pub fn load(path: &Path) -> Option<Saved> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                "{}: {e}; the window opens at its default size",
                path.display()
            );
            return None;
        }
    };
    match serde_json::from_str::<Saved>(&text) {
        Ok(s) if s.valid() => Some(s),
        Ok(s) => {
            tracing::warn!(
                "{}: {} x {} is no window size; the window opens at its default size",
                path.display(),
                s.width,
                s.height
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                "{}: {e}; the window opens at its default size",
                path.display()
            );
            None
        }
    }
}

/// The first inner size for a window opening on `monitor`, and whether it
/// opens maximized: the saved state, else [`DEFAULT`], clamped to the work
/// area. No monitor known: unclamped.
pub fn initial(monitor: Option<&Monitor>, saved: Option<&Saved>) -> ((f64, f64), bool) {
    let want = saved.map_or(DEFAULT, |s| (s.width, s.height));
    let maximized = saved.is_some_and(|s| s.maximized);
    let Some(m) = monitor else {
        return (want, maximized);
    };
    let area = m.work_area().size.to_logical::<f64>(m.scale_factor());
    (fit(want, (area.width, area.height - FRAME)), maximized)
}

/// `want` clamped to `room`; a dimension `room` cannot give a sane value for
/// (a monitor reporting nothing) keeps `want`.
fn fit(want: (f64, f64), room: (f64, f64)) -> (f64, f64) {
    let one = |w: f64, r: f64| if r > 0.0 { w.min(r) } else { w };
    (one(want.0, room.0), one(want.1, room.1))
}

/// Follows the main window's size and writes it to [`file`] (managed state).
pub struct Tracker {
    path: PathBuf,
    state: Mutex<Option<Saved>>,
    /// Bumped by every resize; a pending write goes ahead only while it is
    /// still the latest.
    resizes: AtomicU64,
}

impl Tracker {
    /// The tracker for data dir `dir`, with what was saved there.
    pub fn new(dir: &Path) -> Self {
        let path = file(dir);
        let saved = load(&path);
        Self {
            path,
            state: Mutex::new(saved),
            resizes: AtomicU64::new(0),
        }
    }

    pub fn saved(&self) -> Option<Saved> {
        *self.lock()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Saved>> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Read the window's size and state as they are now. A hidden or
    /// minimized window says nothing; a maximized one keeps the last
    /// unmaximized size.
    fn record(&self, w: &WebviewWindow) {
        if !w.is_visible().unwrap_or(false) || w.is_minimized().unwrap_or(false) {
            return;
        }
        let maximized = w.is_maximized().unwrap_or(false);
        let size = (!maximized)
            .then(|| {
                Some(
                    w.inner_size()
                        .ok()?
                        .to_logical::<f64>(w.scale_factor().ok()?),
                )
            })
            .flatten();
        let mut s = self.lock();
        let (width, height) = match (size, *s) {
            (Some(l), _) => (l.width, l.height),
            (None, Some(old)) => (old.width, old.height),
            // Maximized before any size was seen: the default to come back to.
            (None, None) => DEFAULT,
        };
        *s = Some(Saved {
            width,
            height,
            maximized,
        });
    }

    /// Write the state, if there is one (atomically: a crash mid-write
    /// leaves the old file).
    fn save(&self) {
        let Some(saved) = self.saved() else {
            return;
        };
        let tmp = self.path.with_extension("json.tmp");
        let result = serde_json::to_vec_pretty(&saved)
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(&tmp, json))
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = result {
            tracing::warn!("saving the window size to {}: {e}", self.path.display());
        }
    }

    /// Record and write now (the window is about to hide, or the app exits).
    pub fn flush(&self, w: &WebviewWindow) {
        self.record(w);
        self.save();
    }
}

/// A window event of the app: a resize of the main window is written once it
/// settles, and a close (which hides it) writes at once. Read when the write
/// happens, not from the event: GTK may report a maximize's size before its
/// state.
pub fn on_event(window: &tauri::Window, event: &WindowEvent) {
    if window.label() != LABEL {
        return;
    }
    let app = window.app_handle();
    let Some(tracker) = app.try_state::<Tracker>() else {
        return;
    };
    match event {
        WindowEvent::Resized(_) => {
            let n = tracker.resizes.fetch_add(1, Ordering::SeqCst) + 1;
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(SETTLE).await;
                let tracker = app.state::<Tracker>();
                if tracker.resizes.load(Ordering::SeqCst) != n {
                    return;
                }
                if let Some(w) = app.get_webview_window(LABEL) {
                    tracker.flush(&w);
                }
            });
        }
        WindowEvent::CloseRequested { .. } => {
            if let Some(w) = app.get_webview_window(LABEL) {
                tracker.flush(&w);
            }
        }
        _ => {}
    }
}

/// The app is exiting: write what the window shows now.
pub fn on_exit(app: &tauri::AppHandle) {
    if let (Some(tracker), Some(w)) = (app.try_state::<Tracker>(), app.get_webview_window(LABEL)) {
        tracker.flush(&w);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_big_enough_screen_gets_the_default() {
        // A 2560x1440 desktop, a panel off the height.
        assert_eq!(fit(DEFAULT, (2560.0, 1392.0 - FRAME)), DEFAULT);
        assert_eq!(fit(DEFAULT, (2560.0, 1440.0 - FRAME)), DEFAULT);
    }

    #[test]
    fn a_small_screen_clamps_each_side_on_its_own() {
        assert_eq!(fit(DEFAULT, (1920.0, 1080.0 - FRAME)), (1650.0, 1016.0));
        assert_eq!(fit(DEFAULT, (1366.0, 768.0 - FRAME)), (1366.0, 704.0));
    }

    #[test]
    fn a_monitor_reporting_nothing_keeps_the_size() {
        assert_eq!(fit(DEFAULT, (0.0, -FRAME)), DEFAULT);
    }

    #[test]
    fn no_monitor_takes_the_saved_state_or_the_default() {
        assert_eq!(initial(None, None), (DEFAULT, false));
        let s = Saved {
            width: 2400.0,
            height: 1300.0,
            maximized: true,
        };
        assert_eq!(initial(None, Some(&s)), ((2400.0, 1300.0), true));
    }

    #[test]
    fn the_saved_state_round_trips_and_a_bad_one_is_the_default() {
        let dir = std::env::temp_dir().join(format!("lmgw-window-size-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = file(&dir);
        assert_eq!(load(&path), None, "nothing saved yet");

        let t = Tracker::new(&dir);
        *t.lock() = Some(Saved {
            width: 1800.5,
            height: 1200.0,
            maximized: false,
        });
        t.save();
        assert_eq!(load(&path), t.saved());
        assert_eq!(
            Tracker::new(&dir).saved(),
            t.saved(),
            "the next launch reads it"
        );

        for bad in [
            "not json",
            r#"{"width": 0, "height": 900, "maximized": false}"#,
            r#"{"width": -5, "height": 900, "maximized": false}"#,
            r#"{"width": 1200}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert_eq!(load(&path), None, "{bad}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
