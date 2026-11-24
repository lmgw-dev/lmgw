//! The main window's own frame: on Linux the shell draws the window's
//! decorations itself (CSD) instead of asking KWin for its title bar, so the
//! page can put the window buttons in its own header.
//!
//! Unset, empty or `csd` is that frame. `LMGW_WINDOW=kwin` gives the
//! desktop's own frame back: KWin's server-side title bar (main.rs,
//! `show_main_window`). Any other value is logged and falls back to the
//! default. Only Linux has the CSD frame yet; elsewhere the window keeps its
//! native frame, and an explicit `csd` is logged and ignored.
//!
//! **How the frame is drawn (Linux).** GTK 3 keeps its own client-side
//! decorations, with an empty titlebar widget in place of tao's GtkHeaderBar
//! (`linux.rs`): GTK draws the shadow, keeps the resize edges and the input
//! region, and tells the compositor the window geometry without the shadow.
//! A CSS provider scoped to this window gives the corners the page's radius,
//! the mockup's shadow and a hairline outline, and drops both when the window
//! is maximized, fullscreen or tiled. The webview's own background is
//! transparent; the page rounds its own corners by `--csd-radius` (the shell
//! cannot clip WebKit's drawing, `linux.rs`).
//!
//! **The page's contract** ([`PAGE_JS`], injected at document start):
//! - `<html>` gets the class `csd` and, while they hold, `maximized`,
//!   `fullscreen`, `tiled` (any edge) and `inactive` (unfocused); the
//!   custom property `--csd-radius` is the corner radius in the page's CSS
//!   pixels (the window's radius over the webview's zoom), the radius the
//!   page rounds its root by while the window floats.
//! - `window.lmgwWindow.state` is the latest state (`csd`, `maximized`,
//!   `fullscreen`, `tiled`, `inactive`, `radius`, `zoom`, `layout`); every
//!   change is dispatched as the window event `lmgw-window` with the state as
//!   `detail`.
//!   `layout` is GTK's `gtk-decoration-layout` (KDE syncs its own button
//!   order there): `{start: [...], end: [...]}` with names such as
//!   `minimize`, `maximize`, `close`, `icon`, `menu`; the page places the
//!   buttons it draws by it.
//! - Declarative: an element with `data-window-header` is a header. A press
//!   on it (not on a control inside it) moves the window once dragged past a
//!   few pixels; a double click, a middle click and a right click do what
//!   GTK's titlebar settings say (by default toggle maximize, nothing, the
//!   compositor's window menu). An element with `data-window-button` set to
//!   `minimize`, `maximize` or `close` is that button.
//! - Imperative: `lmgwWindow.minimize()`, `.maximize()` (toggles),
//!   `.close()` (the window's close, which hides it to the tray as before),
//!   `.menu()` (the window menu for the current press), `.radius(px)` (the
//!   corner radius the page draws, in window pixels).
//!
//! All of it goes through one shell command, [`window_chrome`], granted to
//! the gateway's page (capabilities/remote-ui.json) and the demo page
//! (capabilities/window-demo.json). It acts on the main window only.
//!
//! The dashboard draws its own headers and window buttons (`data-window-header`,
//! `data-window-button`) and rounds its own corners. While a page has no
//! visible header (before the WASM has mounted, if it never does, or on a
//! route that draws none) the injected script shows a fallback strip on top
//! with a drag area and the three buttons, and rounds the page's corners, so
//! the window stays usable.

pub(crate) mod demo;
#[cfg(target_os = "linux")]
mod linux;

use serde::{Deserialize, Serialize};

/// The variable that picks the frame.
pub(crate) const WINDOW_VAR: &str = "LMGW_WINDOW";

/// Who draws the main window's frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Frame {
    /// The desktop's own frame: KWin's server-side decorations.
    Kwin,
    /// The shell's own (client-side) decorations, the page's header holding
    /// the window buttons. The default on Linux.
    #[default]
    Csd,
}

/// `LMGW_WINDOW`'s value: unset, empty or `csd` is [`Frame::Csd`], `kwin`
/// is [`Frame::Kwin`] (both case-insensitive, surrounding spaces ignored);
/// anything else is an error naming the value.
pub(crate) fn parse_frame(raw: Option<&str>) -> Result<Frame, String> {
    let Some(raw) = raw else {
        return Ok(Frame::Csd);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "csd" => Ok(Frame::Csd),
        "kwin" => Ok(Frame::Kwin),
        _ => Err(raw.to_string()),
    }
}

/// The frame this process uses, read once: an unknown value is logged once
/// and falls back to the default, and a platform without the CSD frame keeps
/// its native one (logged only when `csd` was asked for by name).
pub(crate) fn frame() -> Frame {
    static FRAME: std::sync::OnceLock<Frame> = std::sync::OnceLock::new();
    *FRAME.get_or_init(|| {
        let raw = std::env::var(WINDOW_VAR).ok();
        let explicit_csd = raw
            .as_deref()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("csd"));
        let parsed = parse_frame(raw.as_deref()).unwrap_or_else(|value| {
            tracing::warn!(
                "{WINDOW_VAR}={value:?} is neither csd nor kwin; the window keeps the default frame"
            );
            Frame::default()
        });
        match parsed {
            Frame::Csd if !cfg!(target_os = "linux") => {
                if explicit_csd {
                    tracing::warn!("{WINDOW_VAR}=csd: only Linux has the shell's own frame yet");
                }
                Frame::Kwin
            }
            Frame::Csd => {
                tracing::info!("the shell draws the window's frame ({WINDOW_VAR}=kwin gives the desktop's title bar)");
                Frame::Csd
            }
            frame => frame,
        }
    })
}

/// What the page asks of the window.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(tag = "do", rename_all = "kebab-case")]
pub(crate) enum Action {
    /// The current [`State`].
    State,
    Minimize,
    /// Maximize, or restore a maximized window.
    Maximize,
    /// The window's close: the shell's close handler hides it to the tray.
    Close,
    /// Move the window with the press that is under way.
    Drag,
    /// A click on the header that is no drag: `button` as GTK counts them
    /// (1 primary, 2 middle, 3 secondary), `count` 2 for a double click.
    Click {
        button: u32,
        count: u32,
    },
    /// The compositor's window menu for the current press.
    Menu,
    /// The corner radius the page draws, in window pixels.
    Radius {
        px: f64,
    },
}

/// GTK's button order: the names before the colon go to the start of the
/// header, the ones after it to the end.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Layout {
    pub start: Vec<String>,
    pub end: Vec<String>,
}

/// `gtk-decoration-layout`, e.g. `icon:minimize,maximize,close`. A layout
/// with no colon is all start, as GTK reads it. Empty names are dropped;
/// names are kept as they are (GTK ignores the ones it does not know, and so
/// may the page).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_layout(raw: &str) -> Layout {
    let names = |s: &str| -> Vec<String> {
        s.split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect()
    };
    let (start, end) = raw.split_once(':').unwrap_or((raw, ""));
    Layout {
        start: names(start),
        end: names(end),
    }
}

/// What the page is told of the window.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct State {
    pub csd: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    /// Tiled at any edge (KWin's quick tiling).
    pub tiled: bool,
    /// Not the focused window.
    pub inactive: bool,
    /// The corner radius, in window pixels.
    pub radius: f64,
    /// The webview's zoom level (the dashboard's interface scale): a CSS
    /// pixel of the page is this many window pixels.
    pub zoom: f64,
    pub layout: Layout,
}

impl State {
    /// The script that hands this state to the page's hook.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn apply_js(&self) -> String {
        let json = serde_json::to_string(self).unwrap_or_else(|_| "null".into());
        format!("window.__lmgwChrome && window.__lmgwChrome({json});")
    }
}

/// The corner radius before the page says its own: the mockup's 14 px at its
/// 125 % scale.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const DEFAULT_RADIUS: f64 = 17.5;

/// A radius the page may set: 0 to 64 window pixels.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn radius_ok(px: f64) -> bool {
    px.is_finite() && (0.0..=64.0).contains(&px)
}

/// The page's end of the contract (module docs), run at document start in
/// the main frame of a CSD window.
pub(crate) const PAGE_JS: &str = include_str!("window_chrome/page.js");

/// The CSD window's content size in logical pixels, as GTK last allocated
/// it (`linux.rs`), readable from any thread. Tauri's inner size of a
/// client-decorated GTK window is its whole surface, shadow included: saved
/// as the size to restore (window_size.rs), the window grew by the shadow at
/// every start. `None` with KWin's frame, where the inner size is the
/// content.
pub(crate) fn content_size() -> Option<tauri::LogicalSize<f64>> {
    *CONTENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static CONTENT: std::sync::Mutex<Option<tauri::LogicalSize<f64>>> = std::sync::Mutex::new(None);

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn set_content_size(size: tauri::LogicalSize<f64>) {
    *CONTENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(size);
}

/// Prepares the window before it is first shown: KWin's frame (`None`) or
/// the shell's own. GTK is only for the main thread and `show_main_window`
/// also runs on others (the single-instance callback runs on zbus's), so the
/// work is handed to the main thread; where the caller is already there it
/// runs at once, before the window is shown.
#[cfg(target_os = "linux")]
pub(crate) fn decorate(window: &tauri::WebviewWindow, frame: Frame) {
    let window_for_main = window.clone();
    let queued = tauri::Manager::app_handle(window).run_on_main_thread(move || {
        let window = window_for_main;
        match frame {
            Frame::Kwin => {
                // tao gives every Wayland window its own GtkHeaderBar, and a
                // window with a custom titlebar is client-decorated: GTK
                // draws the title bar and the shadow itself, in its theme's
                // sizes. With no titlebar GTK asks the compositor to
                // decorate (KDE's server-decoration protocol), so KWin draws
                // the same frame as on every other window: its title bar,
                // buttons, window menu.
                if let Ok(gtk_window) = window.gtk_window() {
                    use ::gtk::prelude::GtkWindowExt;
                    gtk_window.set_titlebar(None::<&::gtk::Widget>);
                }
            }
            Frame::Csd => linux::decorate(&window),
        }
    });
    if let Err(e) = queued {
        tracing::warn!("window frame: could not reach the main thread: {e}");
    }
}

/// The page's window controls (module docs). Synchronous, so Tauri runs it
/// on the main thread, where GTK is.
#[tauri::command]
pub(crate) fn window_chrome(
    window: tauri::WebviewWindow,
    action: Action,
) -> Result<Option<State>, String> {
    if window.label() != "main" {
        return Err("only the main window has these controls".into());
    }
    if frame() != Frame::Csd {
        return match action {
            Action::State => Ok(None),
            _ => Err("the window has KWin's frame; its controls are KWin's".into()),
        };
    }
    #[cfg(target_os = "linux")]
    {
        linux::act(&window, action)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = action;
        Err("only Linux has the shell's own frame yet".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlays_inside_a_header_are_not_the_header() {
        for sel in ["[popover]", "dialog", "[role=menu]"] {
            assert!(PAGE_JS.contains(sel), "{sel} must count as a control");
        }
    }

    #[test]
    fn lmgw_window_defaults_to_csd_reads_kwin_and_nothing_else() {
        assert_eq!(parse_frame(None), Ok(Frame::Csd));
        assert_eq!(parse_frame(Some("")), Ok(Frame::Csd));
        assert_eq!(parse_frame(Some("  ")), Ok(Frame::Csd));
        assert_eq!(parse_frame(Some("kwin")), Ok(Frame::Kwin));
        assert_eq!(parse_frame(Some(" KWin ")), Ok(Frame::Kwin));
        assert_eq!(parse_frame(Some("csd")), Ok(Frame::Csd));
        assert_eq!(parse_frame(Some("CSD")), Ok(Frame::Csd));
        assert_eq!(parse_frame(Some("ssd")), Err("ssd".to_string()));
        assert_eq!(parse_frame(Some("1")), Err("1".to_string()));
    }

    #[test]
    fn the_button_layout_splits_at_the_colon() {
        assert_eq!(
            parse_layout("icon:minimize,maximize,close"),
            Layout {
                start: vec!["icon".into()],
                end: vec!["minimize".into(), "maximize".into(), "close".into()],
            }
        );
        // macOS order, buttons at the start.
        assert_eq!(
            parse_layout("close,minimize,maximize:"),
            Layout {
                start: vec!["close".into(), "minimize".into(), "maximize".into()],
                end: vec![],
            }
        );
        assert_eq!(
            parse_layout("menu"),
            Layout {
                start: vec!["menu".into()],
                end: vec![],
            }
        );
        assert_eq!(
            parse_layout(" : close ,, "),
            Layout {
                start: vec![],
                end: vec!["close".into()],
            }
        );
    }

    #[test]
    fn page_actions_read_as_the_script_sends_them() {
        let a = |s: &str| serde_json::from_str::<Action>(s).unwrap();
        assert_eq!(a(r#"{"do":"state"}"#), Action::State);
        assert_eq!(a(r#"{"do":"maximize"}"#), Action::Maximize);
        assert_eq!(
            a(r#"{"do":"click","button":3,"count":1}"#),
            Action::Click {
                button: 3,
                count: 1
            }
        );
        assert_eq!(
            a(r#"{"do":"radius","px":17.5}"#),
            Action::Radius { px: 17.5 }
        );
        assert!(serde_json::from_str::<Action>(r#"{"do":"resize"}"#).is_err());
    }

    #[test]
    fn a_radius_is_bounded() {
        assert!(radius_ok(0.0) && radius_ok(DEFAULT_RADIUS) && radius_ok(64.0));
        assert!(!radius_ok(-1.0) && !radius_ok(65.0) && !radius_ok(f64::NAN));
    }

    #[test]
    fn the_state_reaches_the_hook_as_json() {
        let s = State {
            csd: true,
            maximized: true,
            radius: 12.0,
            layout: parse_layout(":close"),
            ..State::default()
        };
        let js = s.apply_js();
        assert!(
            js.starts_with("window.__lmgwChrome && window.__lmgwChrome({"),
            "{js}"
        );
        assert!(js.contains(r#""maximized":true"#), "{js}");
        assert!(
            js.contains(r#""layout":{"start":[],"end":["close"]}"#),
            "{js}"
        );
    }
}
