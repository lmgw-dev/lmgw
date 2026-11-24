//! The window's own frame, the page's side (src-tauri/src/window_chrome.rs
//! has the contract): with `LMGW_WINDOW=csd` the shell draws no title bar,
//! and the dashboard's header bars hold the window buttons and move the
//! window.
//!
//! The shell's script (window_chrome/page.js) puts the window's state on
//! `<html>` (`csd`, `maximized`, `fullscreen`, `tiled`, `inactive`, which
//! app.css answers) and in `lmgwWindow.state`, and dispatches each change as
//! the `lmgw-window` event. This module keeps the part the page draws by:
//! whether there is a frame of ours at all and where the desktop puts the
//! buttons (`gtk-decoration-layout`). Every header bar is marked
//! `data-window-header`; the buttons are `data-window-button`, which the
//! script wires. A plain browser and a window in KWin's frame have no state
//! and draw no buttons.
//!
//! The page's corners: the page rounds them by `--csd-radius` (app.css),
//! and tells the shell the radius it draws, [`RADIUS`] CSS pixels in window
//! pixels at the interface scale, so the frame's shadow and outline follow
//! the same curve.

use leptos::prelude::*;
use lmgw_ui_kit::widgets::{Ico, Icon};
use wasm_bindgen::{JsCast, JsValue};

/// The window's corner radius in CSS pixels: the mockup's `--r-win`.
const RADIUS: f64 = 14.0;

/// What the page draws by.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    /// The shell draws no title bar: the page's headers hold the buttons.
    pub csd: bool,
    /// The buttons before the title, in the desktop's order.
    pub start: Vec<String>,
    /// The buttons at the end of the rightmost header.
    pub end: Vec<String>,
}

/// Which end of the header bars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Start,
    End,
}

fn get(o: &JsValue, key: &str) -> JsValue {
    js_sys::Reflect::get(o, &key.into()).unwrap_or(JsValue::UNDEFINED)
}

fn names(o: &JsValue) -> Vec<String> {
    o.dyn_ref::<js_sys::Array>()
        .map(|a| a.iter().filter_map(|v| v.as_string()).collect())
        .unwrap_or_default()
}

/// The page's view of `lmgwWindow.state`; `None` without one.
fn read(state: &JsValue) -> Option<(Frame, f64, f64)> {
    if state.is_undefined() || state.is_null() {
        return None;
    }
    let layout = get(state, "layout");
    let frame = Frame {
        csd: get(state, "csd").as_bool().unwrap_or(false),
        start: names(&get(&layout, "start")),
        end: names(&get(&layout, "end")),
    };
    let radius = get(state, "radius").as_f64().unwrap_or(0.0);
    let zoom = get(state, "zoom")
        .as_f64()
        .filter(|z| *z > 0.0)
        .unwrap_or(1.0);
    Some((frame, radius, zoom))
}

/// The page's `lmgwWindow`, when the shell injected it.
fn api() -> Option<JsValue> {
    let w = get(&window().into(), "lmgwWindow");
    (!w.is_undefined() && !w.is_null()).then_some(w)
}

/// Tell the shell the radius the page draws, when it differs from the one
/// the window has: the frame's shadow and outline take it.
fn sync_radius(radius: f64, zoom: f64) {
    let want = RADIUS * zoom;
    if (radius - want).abs() < 0.25 {
        return;
    }
    let Some(api) = api() else { return };
    if let Ok(f) = get(&api, "radius").dyn_into::<js_sys::Function>() {
        let _ = f.call1(&api, &want.into());
    }
}

/// Install the frame state. Called once from `App`.
pub fn provide_frame() -> RwSignal<Frame> {
    let frame = RwSignal::new(Frame::default());
    let take = move |state: JsValue| {
        if let Some((f, radius, zoom)) = read(&state) {
            if f.csd {
                sync_radius(radius, zoom);
            }
            if frame.get_untracked() != f {
                frame.set(f);
            }
        }
    };
    if let Some(api) = api() {
        take(get(&api, "state"));
    }
    // Lives as long as the app.
    let _ = window_event_listener_untyped("lmgw-window", move |ev| {
        if let Some(c) = ev.dyn_ref::<web_sys::CustomEvent>() {
            take(c.detail());
        }
    });
    provide_context(frame);
    frame
}

pub fn use_frame() -> RwSignal<Frame> {
    expect_context::<RwSignal<Frame>>()
}

/// The window buttons for one end of the header bars, in the desktop's
/// order; nothing without the shell's frame. Names other than minimize,
/// maximize and close (GTK's `icon`, `menu`) draw nothing yet.
#[component]
pub fn WindowButtons(end: End) -> impl IntoView {
    let frame = use_frame();
    let names = Memo::new(move |_| {
        frame.with(|f| {
            if !f.csd {
                return Vec::new();
            }
            match end {
                End::Start => f.start.clone(),
                End::End => f.end.clone(),
            }
        })
    });
    move || {
        let names = names.get();
        let buttons: Vec<_> = names
            .iter()
            .filter_map(|n| {
                let (title, ico) = match n.as_str() {
                    "minimize" => ("Minimize", Ico::WinMin),
                    "maximize" => ("Maximize", Ico::WinMax),
                    "close" => ("Close", Ico::Close),
                    _ => return None,
                };
                Some(view! {
                    <button type="button" class="wbtn" title=title data-window-button=n.clone()>
                        <Icon ico=ico/>
                    </button>
                })
            })
            .collect();
        (!buttons.is_empty()).then(|| {
            let class = match end {
                End::Start => "wctl start",
                End::End => "wctl end",
            };
            view! { <div class=class>{buttons}</div> }
        })
    }
}
