//! What ends a capture without its owner asking (chat-voice §11.2, review
//! M3), and how the owner learns of it.
//!
//! A microphone can stop delivering on its own: the device is unplugged or
//! fails, PipeWire removes its node, the permission is revoked, the user
//! agent mutes the track, the page goes into the back/forward cache, or the
//! browser stops the capture context. None of that may leave a capture
//! "open" that streams or uploads silence while the UI shows the microphone
//! as live. [`Watch`] listens for each and hands the capture a
//! [`CaptureEvent`]: a muted track is reported (and keeps running — the owner
//! decides), an end stops the capture first and is then reported once.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

use super::Inner;

/// What a capture tells its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CaptureEvent {
    /// The user agent muted the track: it delivers silence until
    /// [`CaptureEvent::Unmuted`]. The capture keeps running.
    Muted,
    Unmuted,
    /// The capture ended on its own. It is already stopped (every track, the
    /// graph); nothing more comes.
    Ended(EndReason),
}

/// Why a capture ended without its owner asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EndReason {
    /// The track ended: the device was unplugged or failed, or its node
    /// went away.
    TrackEnded,
    /// The microphone permission was taken back.
    PermissionRevoked,
    /// The page was hidden: left, or put into the back/forward cache (a
    /// page shown again from it starts with every capture closed).
    PageHidden,
    /// The capture context stopped running (closed or interrupted by the
    /// browser): its state.
    ContextStopped(String),
}

impl EndReason {
    /// The page's words for it.
    pub(crate) fn message(&self) -> String {
        match self {
            EndReason::TrackEnded => {
                "the microphone stopped delivering (unplugged, or it failed)".into()
            }
            EndReason::PermissionRevoked => "the microphone permission was taken back".into(),
            EndReason::PageHidden => "the microphone was closed when the page was hidden".into(),
            EndReason::ContextStopped(state) => {
                format!("the browser stopped the microphone's audio context (it is {state})")
            }
        }
    }
}

type Handler = Closure<dyn FnMut()>;

/// The listeners of one capture; [`Watch::detach`] removes them all.
pub(super) struct Watch {
    tracks: Vec<web_sys::MediaStreamTrack>,
    ctx: web_sys::AudioContext,
    /// `ended`, `mute`, `unmute` and the context's `statechange`: kept as
    /// long as the watch (one of them may be what stopped the capture).
    _handlers: [Handler; 4],
    /// The permission's status object and its `change` handler, once the
    /// query answered (it is asynchronous).
    perm: Rc<RefCell<Option<(JsValue, Handler)>>>,
}

impl Watch {
    pub(super) fn install(inner: &Rc<Inner>) -> Watch {
        let tracks: Vec<web_sys::MediaStreamTrack> = inner
            .stream
            .get_audio_tracks()
            .iter()
            .filter_map(|t| t.dyn_into().ok())
            .collect();
        let handler = |inner: &Rc<Inner>, f: fn(&Inner)| {
            let weak: Weak<Inner> = Rc::downgrade(inner);
            Closure::<dyn FnMut()>::new(move || {
                if let Some(me) = weak.upgrade() {
                    f(&me);
                }
            })
        };
        let on_ended = handler(inner, |me| me.end(EndReason::TrackEnded));
        let on_mute = handler(inner, |me| me.mute_changed(true));
        let on_unmute = handler(inner, |me| me.mute_changed(false));
        // Installed once the context runs; nothing here ever suspends it, so
        // any other state is the browser stopping it (closed, interrupted,
        // suspended): nothing would flow, while the UI showed a live mic.
        let on_state = handler(inner, |me| {
            if me.ctx.state() != web_sys::AudioContextState::Running {
                me.end(EndReason::ContextStopped(state_text(&me.ctx)));
            }
        });
        for t in &tracks {
            t.set_onended(Some(on_ended.as_ref().unchecked_ref()));
            t.set_onmute(Some(on_mute.as_ref().unchecked_ref()));
            t.set_onunmute(Some(on_unmute.as_ref().unchecked_ref()));
        }
        inner
            .ctx
            .set_onstatechange(Some(on_state.as_ref().unchecked_ref()));
        let perm = Rc::new(RefCell::new(None));
        watch_permission(Rc::downgrade(inner), perm.clone());
        Watch {
            tracks,
            ctx: inner.ctx.clone(),
            _handlers: [on_ended, on_mute, on_unmute, on_state],
            perm,
        }
    }

    /// Is a track muted by the user agent right now?
    pub(super) fn any_muted(&self) -> bool {
        self.tracks.iter().any(|t| t.muted())
    }

    /// Remove every listener (the capture stopped). The closures stay alive
    /// with the watch.
    pub(super) fn detach(&self) {
        for t in &self.tracks {
            t.set_onended(None);
            t.set_onmute(None);
            t.set_onunmute(None);
        }
        self.ctx.set_onstatechange(None);
        if let Some((status, _)) = self.perm.borrow().as_ref() {
            let _ = js_sys::Reflect::set(status, &"onchange".into(), &JsValue::NULL);
        }
    }
}

/// The context's state as the browser names it (`interrupted` included,
/// which web-sys does not know).
fn state_text(ctx: &web_sys::AudioContext) -> String {
    js_sys::Reflect::get(ctx, &"state".into())
        .ok()
        .and_then(|s| s.as_string())
        .unwrap_or_else(|| "stopped".into())
}

/// `navigator.permissions.query({name: "microphone"})`, where the browser
/// has it: a change to `denied`, or away from `granted`, ends the capture.
/// A browser without it ends the track itself on a revocation.
fn watch_permission(weak: Weak<Inner>, slot: Rc<RefCell<Option<(JsValue, Handler)>>>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let nav: JsValue = window.navigator().into();
    let Some(perms) = js_sys::Reflect::get(&nav, &"permissions".into())
        .ok()
        .filter(|p| p.is_object())
    else {
        return;
    };
    let Some(query) = js_sys::Reflect::get(&perms, &"query".into())
        .ok()
        .and_then(|q| q.dyn_into::<js_sys::Function>().ok())
    else {
        return;
    };
    let desc = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&desc, &"name".into(), &"microphone".into());
    let Ok(promise) = query
        .call1(&perms, &desc)
        .and_then(|p| p.dyn_into::<js_sys::Promise>())
    else {
        return;
    };
    leptos::task::spawn_local(async move {
        let Ok(status) = wasm_bindgen_futures::JsFuture::from(promise).await else {
            return;
        };
        let read = |s: &JsValue| {
            js_sys::Reflect::get(s, &"state".into())
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default()
        };
        let first = read(&status);
        let Some(me) = weak.upgrade() else { return };
        if me.stopped.get() {
            return;
        }
        let watched = status.clone();
        let on_change = Closure::<dyn FnMut()>::new(move || {
            let now = read(&watched);
            if now == "denied" || (first == "granted" && now != "granted") {
                if let Some(me) = weak.upgrade() {
                    me.end(EndReason::PermissionRevoked);
                }
            }
        });
        let _ = js_sys::Reflect::set(&status, &"onchange".into(), on_change.as_ref());
        *slot.borrow_mut() = Some((status, on_change));
    });
}
