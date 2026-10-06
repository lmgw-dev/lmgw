//! Global interface scale — the whole window zooms, the way VS Code does it.
//!
//! The scale is the webview's *own* zoom level (Tauri's `set_webview_zoom`,
//! `webkit_web_view_set_zoom_level` underneath), not a CSS transform and not
//! a rem rewrite of app.css. Everything moves together — the px in the
//! stylesheet, the SVG charts, the viewport units — and every coordinate the
//! UI measures stays in the same space as everything it draws, so no widget
//! needs a scale-aware correction. It is the same mechanism VS Code zooms
//! with, which is why it behaves the same.
//!
//! Only the app shell has a webview to zoom. In a plain browser tab the
//! browser's own zoom is the identical feature, on the identical keys, and
//! already persisted per origin — so there the hotkeys stay unbound and
//! Settings says which knob to reach for instead of offering a dead control.
//!
//! The value is stored as a percent under `lmgw-ui-scale`, and index.html
//! re-applies it before first paint (the same trick as the theme), so a
//! restart comes up already scaled rather than snapping once WASM mounts.

use std::time::Duration;

use leptos::prelude::*;
use leptos::wasm_bindgen::closure::Closure;
use leptos::wasm_bindgen::{JsCast, JsValue};

/// Where the scale lives between runs. Percent, as a plain integer string —
/// index.html parses the same key before the app boots.
const KEY: &str = "lmgw-ui-scale";

/// The rungs the hotkeys walk and the Settings select offers. Browser-style
/// steps rather than VS Code's 1.2^n, because these are the numbers people
/// already read off a zoom menu. The two ends are the whole range there is,
/// and they are both in the dropdown — the limit is shown, not enforced
/// behind the user's back.
const STEPS: [i32; 13] = [50, 67, 75, 80, 90, 100, 110, 125, 150, 175, 200, 250, 300];

/// The app window's scale until one is picked, and what Ctrl+0 returns to:
/// 125%: the window is read from further away than a browser tab, beside
/// apps in the desktop's own sizes. index.html's first-paint script carries
/// the same number.
const DEFAULT: i32 = 125;

pub fn steps() -> &'static [i32] {
    &STEPS
}

fn clamp(pct: i32) -> i32 {
    pct.clamp(STEPS[0], STEPS[STEPS.len() - 1])
}

#[derive(Clone, Copy)]
pub struct UiScale {
    /// Percent; 100 is unscaled.
    pub pct: RwSignal<i32>,
    hud: RwSignal<bool>,
    /// Bumped per flash, so an earlier timeout cannot hide a later readout.
    flash_gen: RwSignal<u32>,
}

impl UiScale {
    pub fn set(&self, pct: i32) {
        self.pct.set(clamp(pct));
    }

    /// One rung up (`dir > 0`) or down. Reads as "the next rung past where we
    /// are", so a hand-edited in-between value still steps somewhere sane.
    fn nudge(&self, dir: i32) {
        let cur = self.pct.get_untracked();
        let next = if dir > 0 {
            STEPS.iter().copied().find(|s| *s > cur)
        } else {
            STEPS.iter().copied().rev().find(|s| *s < cur)
        };
        self.pct.set(next.unwrap_or(clamp(cur)));
        self.flash();
    }

    /// Show the readout for a beat. Only the hotkeys call this — in Settings
    /// the number is already on screen.
    fn flash(&self) {
        let n = self.flash_gen.get_untracked().wrapping_add(1);
        self.flash_gen.set(n);
        self.hud.set(true);
        let (hud, flash_gen) = (self.hud, self.flash_gen);
        set_timeout(
            move || {
                if flash_gen.get_untracked() == n {
                    hud.set(false);
                }
            },
            Duration::from_millis(1100),
        );
    }
}

pub fn use_ui_scale() -> UiScale {
    expect_context::<UiScale>()
}

/// True inside the Tauri shell, where there is a webview zoom to drive.
pub fn supported() -> bool {
    tauri_invoke().is_some()
}

/// Install the scale context, apply what was stored, and — in the shell —
/// bind the zoom gestures. Called once from `App`.
pub fn provide_ui_scale() {
    let scale = UiScale {
        pct: RwSignal::new(stored()),
        hud: RwSignal::new(false),
        flash_gen: RwSignal::new(0),
    };
    provide_context(scale);
    // One place applies and persists, whichever surface changed the value:
    // the Settings select, a hotkey, or the wheel.
    Effect::new(move |_| {
        let pct = scale.pct.get();
        apply(pct);
        if let Ok(Some(store)) = window().local_storage() {
            let _ = store.set_item(KEY, &pct.to_string());
        }
    });
    if supported() {
        install_gestures(scale);
    }
}

/// Transient readout after a hotkey. The whole window changes at once, so
/// without a number on screen there is nothing to say where you landed.
#[component]
pub fn ZoomHud() -> impl IntoView {
    let scale = use_ui_scale();
    view! {
        <div class="zoom-hud" class:on=move || scale.hud.get() aria-hidden="true">
            {move || format!("{}%", scale.pct.get())}
        </div>
    }
}

fn stored() -> i32 {
    window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item(KEY).ok().flatten())
        .and_then(|v| v.parse::<i32>().ok())
        .map(clamp)
        .unwrap_or(DEFAULT)
}

/// The shell's IPC entry point, as `(this, invoke)` — absent in a browser.
///
/// Shared rather than private: the zoom is not the only thing that has to talk
/// to the shell. The schema form's `directory`/`file` control opens the system
/// file picker over this very bridge (`plugin:dialog|open`, mounts §5.4), and
/// `None` here is exactly how it learns there is no picker to open — one test
/// for "are we inside the shell", not two that could disagree.
pub(crate) fn tauri_invoke() -> Option<(JsValue, js_sys::Function)> {
    let win: JsValue = window().into();
    let internals = js_sys::Reflect::get(&win, &JsValue::from_str("__TAURI_INTERNALS__")).ok()?;
    if !internals.is_object() {
        return None;
    }
    let invoke = js_sys::Reflect::get(&internals, &JsValue::from_str("invoke")).ok()?;
    Some((internals, invoke.dyn_into::<js_sys::Function>().ok()?))
}

fn apply(pct: i32) {
    let Some((internals, invoke)) = tauri_invoke() else {
        return;
    };
    let args = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &args,
        &JsValue::from_str("value"),
        &JsValue::from_f64(f64::from(pct) / 100.0),
    );
    // Fire and forget: the promise only rejects when the capability is
    // missing, and an unhandled rejection in the console is exactly the
    // signal we would want in that case.
    let _ = invoke.call2(
        &internals,
        &JsValue::from_str("plugin:webview|set_webview_zoom"),
        &args,
    );
}

/// Ctrl +/−/0 and Ctrl+wheel, bound on the window so they work wherever the
/// focus happens to be. Shell only — in a browser these keys are the
/// browser's, and stealing them would replace a working zoom with ours.
fn install_gestures(scale: UiScale) {
    let keys =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |ev: web_sys::KeyboardEvent| {
            if !(ev.ctrl_key() || ev.meta_key()) || ev.alt_key() {
                return;
            }
            // `+` and `_` are the shifted faces of the same two keys; a German
            // layout hands us `+` unshifted. Match the character, not the code.
            match ev.key().as_str() {
                "+" | "=" => scale.nudge(1),
                "-" | "_" => scale.nudge(-1),
                "0" => {
                    scale.set(DEFAULT);
                    scale.flash();
                }
                _ => return,
            }
            ev.prevent_default();
        });
    let _ = window().add_event_listener_with_callback("keydown", keys.as_ref().unchecked_ref());
    keys.forget();

    // A trackpad emits a stream of tiny deltas, so one rung per gesture tick
    // rather than per event — otherwise a flick crosses the whole ladder.
    let last = RwSignal::new(0.0_f64);
    let wheel = Closure::<dyn FnMut(web_sys::WheelEvent)>::new(move |ev: web_sys::WheelEvent| {
        if !(ev.ctrl_key() || ev.meta_key()) || ev.delta_y() == 0.0 {
            return;
        }
        // Always: whatever we do with it, Ctrl+wheel must not also scroll.
        ev.prevent_default();
        let now = js_sys::Date::now();
        if now - last.get_untracked() < 80.0 {
            return;
        }
        last.set(now);
        scale.nudge(if ev.delta_y() < 0.0 { 1 } else { -1 });
    });
    // Explicitly non-passive: a wheel listener on the window is passive by
    // default, and a passive one cannot call preventDefault.
    let opts = web_sys::AddEventListenerOptions::new();
    opts.set_passive(false);
    let _ = window().add_event_listener_with_callback_and_add_event_listener_options(
        "wheel",
        wheel.as_ref().unchecked_ref(),
        &opts,
    );
    wheel.forget();
}
