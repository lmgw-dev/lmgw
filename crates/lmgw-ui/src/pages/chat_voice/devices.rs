//! The window's audio devices (chat-voice §2.4, §12): the body of the
//! composer's voice menu — the input and output lists, a level meter, a
//! test tone, and the echo mode with §12.2's warning.
//!
//! [`VoiceDevices`] is the window's choice as the page holds it, provided by
//! the Chat page, so the popover, dictation and the realtime panel's echo
//! chip read one value. Each change is stored at once (`localStorage`, see
//! [`super::audio`]) and an output change is applied to the playing context.
//! For as long as the page lives, a device coming or going re-reads the
//! lists and re-applies the route, and a change made in another tab of this
//! browser profile is taken over.
//!
//! The composer's voice menu button is the one place to see that something
//! is off with lmgw's audio: the output route failed, the chosen output is
//! gone, the playback did not start, or §12.2's echo warning
//! ([`VoiceDevices::alert`]).

mod echo;
mod meter;
/// The meters' animation loop, which dictation's level meter runs too.
pub(super) use meter::run as run_meter;
mod panel;
/// The popover's body: the composer's voice menu holds it (`controls/menu.rs`).
pub(crate) use panel::DevicesPanel;

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

use super::audio::devices::{echo_warning, resolve, DevicePick, EchoMode, Resolved};
use super::audio::listing::{self, Listing};
use super::audio::player::Route;
use super::audio::{self, devices, player};

pub(crate) use echo::EchoChip;

/// The window's device choice and echo mode, with the last device listing.
#[derive(Clone, Copy)]
pub(crate) struct VoiceDevices {
    pub input: RwSignal<Option<DevicePick>>,
    pub output: RwSignal<Option<DevicePick>>,
    pub echo: RwSignal<EchoMode>,
    /// The last listing read (`None` before the first).
    pub listing: RwSignal<Option<Listing>>,
    /// Bumped per listing read; only the newest one lands.
    reads: RwSignal<u64>,
}

impl VoiceDevices {
    fn new() -> Self {
        Self {
            input: RwSignal::new(audio::stored_input()),
            output: RwSignal::new(audio::stored_output()),
            echo: RwSignal::new(audio::stored_echo()),
            listing: RwSignal::new(None),
            reads: RwSignal::new(0),
        }
    }

    pub(crate) fn set_input(&self, pick: Option<DevicePick>) {
        audio::store_pick(devices::KEY_INPUT, pick.as_ref());
        self.input.set(pick);
    }

    /// Choose the output, and apply it to a playing context now.
    pub(crate) fn set_output(&self, pick: Option<DevicePick>) {
        audio::store_pick(devices::KEY_OUTPUT, pick.as_ref());
        self.output.set(pick);
        if let Some(p) = player::existing() {
            p.reroute();
        }
    }

    pub(crate) fn set_echo(&self, mode: EchoMode) {
        audio::store_echo(mode);
        self.echo.set(mode);
    }

    /// Read the device lists again.
    pub(crate) fn refresh(&self) {
        let n = self.reads.get_untracked() + 1;
        self.reads.set(n);
        let (reads, listing) = (self.reads, self.listing);
        leptos::task::spawn_local(async move {
            let l = audio::listing::list().await;
            // A disposed page drops the answer (a write there is a no-op,
            // but the read is not).
            if reads.try_get_untracked() == Some(n) {
                listing.set(Some(l));
            }
        });
    }

    /// The input as resolved against the listing (tracked).
    pub(crate) fn input_resolved(&self) -> Resolved {
        let pick = self.input.get();
        self.listing.with(|l| match l {
            Some(l) => resolve(pick.as_ref(), &l.inputs, l.inputs_known),
            None => resolve(pick.as_ref(), &[], false),
        })
    }

    /// The output as resolved against the listing (tracked).
    pub(crate) fn output_resolved(&self) -> Resolved {
        let pick = self.output.get();
        self.listing.with(|l| match l {
            Some(l) => resolve(pick.as_ref(), &l.outputs, l.outputs_known),
            None => resolve(pick.as_ref(), &[], false),
        })
    }

    /// §12.2's echo warning, when it applies (tracked).
    pub(crate) fn warning(&self) -> Option<String> {
        echo_warning(self.echo.get(), &self.output_resolved())
    }

    /// What the composer's devices button warns about, most pressing first
    /// (tracked): lmgw's playback failed to start, its output route failed,
    /// the chosen output is gone, or the echo warning (review m5).
    pub(crate) fn alert(&self) -> Option<String> {
        let st = player::status();
        let player = st.with(|s| {
            s.error
                .as_ref()
                .map(|e| format!("lmgw's playback: {e}"))
                .or_else(|| match &s.route {
                    Route::Failed(e) => Some(format!("lmgw's output: {e}")),
                    _ => None,
                })
        });
        player
            .or_else(|| self.output_resolved().note("output"))
            .or_else(|| self.warning())
    }

    /// Take over a choice another tab of this browser profile stored.
    fn reread(&self) {
        let input = audio::stored_input();
        if self.input.get_untracked() != input {
            self.input.set(input);
        }
        let echo = audio::stored_echo();
        if self.echo.get_untracked() != echo {
            self.echo.set(echo);
        }
        let output = audio::stored_output();
        if self.output.get_untracked() != output {
            self.output.set(output);
            if let Some(p) = player::existing() {
                p.reroute();
            }
        }
    }
}

/// The window's devices: the Chat page provides them once, and they watch
/// the devices and the other tabs for as long as it lives.
pub(crate) fn provide_voice_devices() -> VoiceDevices {
    let d = VoiceDevices::new();
    provide_context(d);
    watch_page(d);
    d
}

/// `devicechange` (a device came or went: the lists are read again and the
/// route re-applied, so a replugged output gets lmgw back and a gone one is
/// said) and `storage` (another tab changed the choice), for the page's life
/// (review m7, n11). A capture whose own device goes ends itself.
fn watch_page(dev: VoiceDevices) {
    let on_device = Closure::<dyn FnMut()>::new(move || {
        dev.refresh();
        if let Some(p) = player::existing() {
            p.reroute();
        }
    });
    let md = listing::media_devices();
    if let Some(md) = &md {
        let _ =
            md.add_event_listener_with_callback("devicechange", on_device.as_ref().unchecked_ref());
    }
    let on_storage = Closure::<dyn FnMut(JsValue)>::new(move |ev: JsValue| {
        let key = js_sys::Reflect::get(&ev, &"key".into())
            .ok()
            .and_then(|k| k.as_string());
        // `null`: the whole storage was cleared.
        if key.as_deref().is_none_or(|k| k.starts_with("lmgw.voice.")) {
            dev.reread();
        }
    });
    let window = web_sys::window();
    if let Some(w) = &window {
        let _ = w.add_event_listener_with_callback("storage", on_storage.as_ref().unchecked_ref());
    }
    let held = StoredValue::new_local((md, window, on_device, on_storage));
    on_cleanup(move || {
        held.with_value(|(md, w, on_device, on_storage)| {
            if let Some(md) = md {
                let _ = md.remove_event_listener_with_callback(
                    "devicechange",
                    on_device.as_ref().unchecked_ref(),
                );
            }
            if let Some(w) = w {
                let _ = w.remove_event_listener_with_callback(
                    "storage",
                    on_storage.as_ref().unchecked_ref(),
                );
            }
        });
    });
}

/// The window's devices, from the page's context (made here if the page
/// did not provide them).
pub(crate) fn use_voice_devices() -> VoiceDevices {
    use_context::<VoiceDevices>().unwrap_or_else(provide_voice_devices)
}
