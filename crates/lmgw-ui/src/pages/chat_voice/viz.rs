//! The visualisation adapter (chat-voice §10): a canvas in the realtime
//! panel, the variant's ES module imported by URL (`/voice/viz/<v>.js`,
//! copied by Trunk beside the worklets), and the §10 handle fed from the
//! panel: `setState` from §9.3, the analysers as they come and go
//! (`setInputs`), `setTiming`, `resize` from the box's size, `destroy` on
//! cleanup and on a variant switch.
//!
//! **All three variants stay selectable** (the owner's ruling): a setting of
//! this window (`localStorage` `lmgw.voice.viz`), one for the in-chat panel
//! and one for the larger focus view — the ribbon and the orb by default. A
//! switch makes a fresh canvas: one that held the orb's WebGL2 context gives
//! no 2D context to the ring.

use leptos::html;
use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use super::realtime::VoiceState;

#[wasm_bindgen(inline_js = "export function lmgw_import(url) { return import(url); }")]
extern "C" {
    /// The browser's `import()`: a module by URL, at runtime.
    fn lmgw_import(url: &str) -> js_sys::Promise;
}

/// The three variants of the owner's sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Variant {
    Ribbon,
    Orb,
    Ring,
}

impl Variant {
    pub(crate) const ALL: [Variant; 3] = [Variant::Ribbon, Variant::Orb, Variant::Ring];

    pub(crate) fn key(self) -> &'static str {
        match self {
            Variant::Ribbon => "ribbon",
            Variant::Orb => "orb",
            Variant::Ring => "ring",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Variant::Ribbon => "Ribbon",
            Variant::Orb => "Orb",
            Variant::Ring => "Ring",
        }
    }

    pub(crate) fn hint(self) -> &'static str {
        match self {
            Variant::Ribbon => "a mirrored waveform: widest and calmest, it fits a short panel",
            Variant::Orb => "a glowing body that swells with the voice (WebGL2, a 2D fallback)",
            Variant::Ring => "the voice's frequency bands around a circle",
        }
    }

    fn url(self) -> String {
        format!("/voice/viz/{}.js", self.key())
    }
}

/// The window's choice, per view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VizChoice {
    pub panel: Variant,
    pub focus: Variant,
}

impl Default for VizChoice {
    /// The owner's ruling: the ribbon in the chat panel, the orb in the
    /// larger focus view.
    fn default() -> Self {
        VizChoice {
            panel: Variant::Ribbon,
            focus: Variant::Orb,
        }
    }
}

pub(crate) const KEY_VIZ: &str = "lmgw.voice.viz";

impl VizChoice {
    /// A stored value, read tolerantly: each view falls back on its own.
    pub(crate) fn parse(raw: &str) -> Self {
        let d = VizChoice::default();
        let v: serde_json::Value = serde_json::from_str(raw).unwrap_or_default();
        let pick = |k: &str, fallback: Variant| {
            serde_json::from_value::<Variant>(v[k].clone()).unwrap_or(fallback)
        };
        VizChoice {
            panel: pick("panel", d.panel),
            focus: pick("focus", d.focus),
        }
    }

    pub(crate) fn of(&self, focus: bool) -> Variant {
        if focus {
            self.focus
        } else {
            self.panel
        }
    }

    pub(crate) fn with(mut self, focus: bool, v: Variant) -> Self {
        if focus {
            self.focus = v;
        } else {
            self.panel = v;
        }
        self
    }
}

pub(crate) fn stored_choice() -> VizChoice {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(KEY_VIZ).ok().flatten())
        .map(|raw| VizChoice::parse(&raw))
        .unwrap_or_default()
}

pub(crate) fn store_choice(c: VizChoice) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = s.set_item(KEY_VIZ, &serde_json::to_string(&c).unwrap_or_default());
    }
}

/// What the panel feeds the visualisation.
#[derive(Clone, Copy)]
pub(crate) struct VizFeed {
    pub state: Signal<VoiceState>,
    pub muted: Signal<bool>,
    pub loading: Signal<Option<String>>,
    pub held: Signal<bool>,
    pub output: RwSignal<Option<web_sys::AnalyserNode>, LocalStorage>,
    pub input: RwSignal<Option<web_sys::AnalyserNode>, LocalStorage>,
    pub timing: Signal<Option<serde_json::Value>>,
    pub variant: Signal<Variant>,
}

fn call(h: &JsValue, method: &str, args: &[JsValue]) {
    let Ok(f) = js_sys::Reflect::get(h, &JsValue::from_str(method)) else {
        return;
    };
    let Some(f) = f.dyn_ref::<js_sys::Function>() else {
        return;
    };
    let arr = js_sys::Array::new();
    for a in args {
        arr.push(a);
    }
    if let Err(e) = f.apply(h, &arr) {
        leptos::logging::warn!(
            "voice visualisation: {method} failed: {}",
            super::audio::shell::js_text(&e)
        );
    }
}

fn obj(fields: &[(&str, JsValue)]) -> JsValue {
    let o = js_sys::Object::new();
    for (k, v) in fields {
        let _ = js_sys::Reflect::set(&o, &JsValue::from_str(k), v);
    }
    o.into()
}

fn node(a: &Option<web_sys::AnalyserNode>) -> JsValue {
    a.as_ref()
        .map(|a| a.clone().into())
        .unwrap_or(JsValue::NULL)
}

fn json(v: &serde_json::Value) -> JsValue {
    js_sys::JSON::parse(&v.to_string()).unwrap_or(JsValue::NULL)
}

/// The canvas and its module. `palette_from` is the element whose `--rt-*`
/// tokens are the colours (the panel).
#[component]
pub(crate) fn Viz(feed: VizFeed) -> impl IntoView {
    let host: NodeRef<html::Div> = NodeRef::new();
    let handle = StoredValue::new_local(None::<JsValue>);
    // Bumped per mount and at cleanup: an import that lands late is
    // destroyed at once.
    let gen = StoredValue::new(0u64);
    let kind = RwSignal::new(String::new());
    // A module that did not load or mount: said on the stage, not only in
    // the console (WP11 UI review NIT 8).
    let failed = RwSignal::new(None::<String>);
    let size = StoredValue::new((0.0f64, 0.0f64));
    // Reduced motion as the system says it now: a change while the panel is
    // open mounts the variant afresh with it (review NIT 8).
    let reduced = RwSignal::new(false);
    if let Some(mq) = window()
        .match_media("(prefers-reduced-motion: reduce)")
        .ok()
        .flatten()
    {
        reduced.set(mq.matches());
        let on = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
            let now = window()
                .match_media("(prefers-reduced-motion: reduce)")
                .ok()
                .flatten()
                .is_some_and(|m| m.matches());
            reduced.try_set(now);
        });
        let _ = mq.add_event_listener_with_callback("change", on.as_ref().unchecked_ref());
        let held = StoredValue::new_local(Some((mq, on)));
        on_cleanup(move || {
            if let Some((mq, on)) = held.try_update_value(Option::take).flatten() {
                let _ =
                    mq.remove_event_listener_with_callback("change", on.as_ref().unchecked_ref());
            }
        });
    }

    let info = move || {
        obj(&[
            ("since", JsValue::from_f64(js_sys::Date::now())),
            ("muted", JsValue::from_bool(feed.muted.get_untracked())),
            (
                "loading",
                feed.loading
                    .get_untracked()
                    .map(|l| JsValue::from_str(&l))
                    .unwrap_or(JsValue::NULL),
            ),
            ("held", JsValue::from_bool(feed.held.get_untracked())),
        ])
    };
    let destroy = move || {
        if let Some(h) = handle.try_update_value(Option::take).flatten() {
            call(&h, "destroy", &[]);
        }
    };

    // A fresh canvas per variant, mounted once it is in the page.
    Effect::new(move |_| {
        let variant = feed.variant.get();
        let reduced = reduced.get();
        let Some(host_el) = host.get() else { return };
        destroy();
        failed.set(None);
        let n = gen.get_value() + 1;
        gen.set_value(n);
        host_el.set_inner_html("");
        let Ok(canvas) = document().create_element("canvas") else {
            return;
        };
        canvas.set_attribute("data-viz", variant.key()).ok();
        let _ = host_el.append_child(&canvas);
        let panel = host_el
            .closest("[data-rt-panel]")
            .ok()
            .flatten()
            .unwrap_or_else(|| host_el.clone().into());
        let palette = json(&super::realtime::palette(&panel));
        let inputs = obj(&[
            ("output", node(&feed.output.get_untracked())),
            ("input", node(&feed.input.get_untracked())),
            ("palette", palette),
            ("reducedMotion", JsValue::from_bool(reduced)),
        ]);
        let state = feed.state.get_untracked();
        let url = variant.url();
        leptos::task::spawn_local(async move {
            let fail = move |what: String| {
                leptos::logging::warn!("voice visualisation: {what}");
                if gen.try_get_value() == Some(n) {
                    kind.try_set("failed".into());
                    failed.try_set(Some(what));
                }
            };
            let module = match wasm_bindgen_futures::JsFuture::from(lmgw_import(&url)).await {
                Ok(m) => m,
                Err(e) => {
                    return fail(format!(
                        "{url} did not load: {}",
                        super::audio::shell::js_text(&e)
                    ))
                }
            };
            if gen.try_get_value() != Some(n) {
                return;
            }
            let mount = js_sys::Reflect::get(&module, &JsValue::from_str("mount"))
                .ok()
                .and_then(|m| m.dyn_into::<js_sys::Function>().ok());
            let Some(mount) = mount else {
                return fail(format!("{url} has no mount()"));
            };
            let h = match mount.call2(&JsValue::NULL, &canvas, &inputs) {
                Ok(h) => h,
                Err(e) => {
                    return fail(format!(
                        "{url} did not mount: {}",
                        super::audio::shell::js_text(&e)
                    ))
                }
            };
            let (w, hh) = size.get_value();
            if w > 0.0 {
                call(
                    &h,
                    "resize",
                    &[
                        JsValue::from_f64(w),
                        JsValue::from_f64(hh),
                        JsValue::from_f64(window().device_pixel_ratio()),
                    ],
                );
            }
            call(&h, "setState", &[JsValue::from_str(state.key()), info()]);
            let k = js_sys::Reflect::get(&h, &JsValue::from_str("kind"))
                .ok()
                .and_then(|k| k.as_string())
                .unwrap_or_default();
            kind.try_set(k);
            handle.set_value(Some(h));
        });
    });

    // The box's size: the canvas follows it, at the device's pixel ratio.
    Effect::new(move |_| {
        let Some(el) = host.get() else { return };
        let cb = Closure::<dyn FnMut(js_sys::Array)>::new(move |entries: js_sys::Array| {
            let Some(e) = entries
                .get(0)
                .dyn_into::<web_sys::ResizeObserverEntry>()
                .ok()
            else {
                return;
            };
            let r = e.content_rect();
            size.set_value((r.width(), r.height()));
            handle.with_value(|h| {
                if let Some(h) = h {
                    call(
                        h,
                        "resize",
                        &[
                            JsValue::from_f64(r.width()),
                            JsValue::from_f64(r.height()),
                            JsValue::from_f64(window().device_pixel_ratio()),
                        ],
                    );
                }
            });
        });
        let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) else {
            return;
        };
        ro.observe(&el);
        let held = StoredValue::new_local(Some((ro, cb)));
        on_cleanup(move || {
            if let Some((ro, _cb)) = held.try_update_value(Option::take).flatten() {
                ro.disconnect();
            }
        });
    });

    // §9.3's state, and the flags beside it.
    Effect::new(move |_| {
        let s = feed.state.get();
        let _ = (feed.muted.get(), feed.loading.get(), feed.held.get());
        handle.with_value(|h| {
            if let Some(h) = h {
                call(h, "setState", &[JsValue::from_str(s.key()), info()]);
            }
        });
    });
    // The analysers as they come and go (the microphone opens after the
    // mount; a device change reopens it).
    Effect::new(move |_| {
        let out = feed.output.get();
        let inp = feed.input.get();
        handle.with_value(|h| {
            if let Some(h) = h {
                call(
                    h,
                    "setInputs",
                    &[obj(&[("output", node(&out)), ("input", node(&inp))])],
                );
            }
        });
    });
    Effect::new(move |_| {
        let t = feed.timing.get();
        handle.with_value(|h| {
            if let Some(h) = h {
                call(
                    h,
                    "setTiming",
                    &[t.as_ref().map(json).unwrap_or(JsValue::NULL)],
                );
            }
        });
    });
    on_cleanup(move || {
        gen.try_update_value(|g| *g += 1);
        destroy();
    });

    view! {
        <div class="rt-viz" node_ref=host data-viz-kind=move || kind.get()></div>
        {move || failed.get().map(|why| view! {
            <p class="rt-viz-failed" data-viz-failed="">
                {format!("The visualisation did not load ({why}). Voice mode works without it.")}
            </p>
        })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_choice_defaults_to_the_rulings_and_reads_tolerantly() {
        let d = VizChoice::default();
        assert_eq!((d.panel, d.focus), (Variant::Ribbon, Variant::Orb));
        let c = VizChoice::parse(r#"{"panel":"ring","focus":"nonsense"}"#);
        assert_eq!((c.panel, c.focus), (Variant::Ring, Variant::Orb));
        assert_eq!(VizChoice::parse("garbage"), d);
        let c = d.with(true, Variant::Ribbon);
        assert_eq!(c.of(true), Variant::Ribbon);
        assert_eq!(c.of(false), Variant::Ribbon);
        let back = VizChoice::parse(&serde_json::to_string(&d.with(false, Variant::Orb)).unwrap());
        assert_eq!(back.panel, Variant::Orb);
    }
}
