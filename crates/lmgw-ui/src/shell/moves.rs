//! The desktop shell's moves (machine-check design §2.4, D162): the tray's
//! "Graphics card needs attention…" sends the window to
//! `/settings/gpu#machine`. The shell does not reload the window for it:
//! it fires a cancelable `lmgw-navigate` event on the page's `window` with
//! `{to}` (`show_main_window_at` in `src-tauri/src/main.rs`), and this
//! listener moves the router there, so what the page holds (a setup's
//! progress, an unsaved draft) stays. Cancelling the event tells the shell
//! the page took it; a document that does not (the login page, an agent's
//! app, a page still loading) is loaded at the route instead.
//!
//! A draft the move would lose is asked about first, as a link would be.

use leptos::prelude::*;
use leptos_router::hooks::use_navigate;
use leptos_router::NavigateOptions;
use wasm_bindgen::JsValue;

/// The shell's event.
pub const EVENT: &str = "lmgw-navigate";

/// How many moves the shell made: a page whose address the move did not
/// change (the card is already open) tracks it to scroll in again.
#[derive(Clone, Copy)]
pub struct ShellMoves(RwSignal<u64>);

impl ShellMoves {
    pub fn track(self) {
        self.0.track();
    }
}

/// Install the counter. Call once, in `App`, outside the router; render
/// [`ShellNavigate`] inside it.
pub fn provide_shell_moves() {
    provide_context(ShellMoves(RwSignal::new(0)));
}

/// A route of the dashboard's own: a path on this origin, never another
/// origin's URL (`//host`) or a scheme.
pub(crate) fn in_app_route(to: &str) -> bool {
    to.starts_with('/') && !to.starts_with("//") && !to.contains('\\')
}

/// The listener.
#[component]
pub fn ShellNavigate() -> impl IntoView {
    let navigate = use_navigate();
    let moves = use_context::<ShellMoves>();
    let guard = crate::widgets::dirty_guard::try_use_dirty_guard();
    let listener = window_event_listener_untyped(EVENT, move |ev| {
        let to = js_sys::Reflect::get(&ev, &JsValue::from_str("detail"))
            .ok()
            .and_then(|d| js_sys::Reflect::get(&d, &JsValue::from_str("to")).ok())
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        if !in_app_route(&to) {
            return;
        }
        // Taken: the shell does not load the page anew.
        ev.prevent_default();
        if guard.is_some_and(|g| g.ask_before(&to)) {
            return;
        }
        navigate(&to, NavigateOptions::default());
        if let Some(m) = moves {
            m.0.update(|n| *n += 1);
        }
    });
    on_cleanup(move || listener.remove());
}

#[cfg(test)]
mod tests {
    use super::in_app_route;

    #[test]
    fn only_the_dashboards_own_routes_are_taken() {
        assert!(in_app_route("/settings/gpu#machine"));
        assert!(in_app_route("/"));
        assert!(!in_app_route(""));
        assert!(!in_app_route("//evil.example/x"));
        assert!(!in_app_route("/\\evil.example"));
        assert!(!in_app_route("https://evil.example/"));
        assert!(!in_app_route("javascript:alert(1)"));
    }
}
