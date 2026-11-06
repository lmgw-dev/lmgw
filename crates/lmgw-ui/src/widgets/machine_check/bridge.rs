//! The shell's `machine_apply` command (machine-check design §4.3, WP7's
//! contract for WP8a), over the same `window.__TAURI_INTERNALS__` bridge the
//! zoom and the audio commands use ([`crate::ui_scale::tauri_invoke`]).
//!
//! Progress comes over a Tauri `Channel` passed as the command's own
//! argument, made the way `@tauri-apps/api`'s `Channel` makes one: a callback
//! registered with `transformCallback`, sent as the string `"__CHANNEL__:" +
//! id`. Tauri calls it with `{ message, index }` per event (a message above
//! 8 KB is fetched by Tauri's own glue first, so it arrives the same way) and
//! once with `{ end: true, index }`; [`Ordered`] hands the messages on in
//! `index` order, as `Channel` does.
//!
//! The channel's events go to whatever document the window runs (the
//! review's L6): after a reload the new document has no callback of that id,
//! and Tauri says so on the console, which is not the page's error. A
//! component that is gone ignores what still reaches it: its [`Listener`] is
//! unregistered when the component is cleaned up, and the handler it was
//! given checks the component's scope first.

use std::collections::BTreeMap;

use lmgw_api_types::machine::{MachineApplyEvent, MachineApplyResult};
use serde::Deserialize;
use serde_json::Value;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// Is the page inside the app's own window? The one test the dashboard uses
/// for every shell-only control: `__TAURI_INTERNALS__` is there. In a browser
/// the fix has no button (§2.7).
pub(crate) fn in_app() -> bool {
    crate::ui_scale::tauri_invoke().is_some()
}

/// One frame as Tauri's channel glue hands it to the callback.
#[derive(Debug, Deserialize)]
struct Frame {
    index: usize,
    #[serde(default)]
    end: bool,
    #[serde(default)]
    message: Option<Value>,
}

/// The channel's frames in `index` order (what `Channel` in
/// `@tauri-apps/api` does): a message that arrives early waits for the ones
/// before it, and the end counts only once every message before it is in.
#[derive(Debug, Default)]
pub(crate) struct Ordered {
    next: usize,
    held: BTreeMap<usize, Value>,
    end: Option<usize>,
}

impl Ordered {
    /// A message at `index`: every message that is now due, in order.
    pub(crate) fn message(&mut self, index: usize, message: Value) -> Vec<Value> {
        if index < self.next {
            return Vec::new();
        }
        self.held.insert(index, message);
        let mut due = Vec::new();
        while let Some(m) = self.held.remove(&self.next) {
            due.push(m);
            self.next += 1;
        }
        due
    }

    /// The end, sent with the number of messages there were.
    pub(crate) fn end(&mut self, index: usize) {
        self.end = Some(index);
    }

    /// Every message the channel sent has been handed on, and it ended.
    pub(crate) fn done(&self) -> bool {
        self.end.is_some_and(|e| self.next >= e)
    }
}

/// The registered callback: its id, and the closure Tauri calls. Kept by
/// the component that made it and [`Listener::stop`]ped when that is cleaned
/// up or a new run replaces it, never from inside its own call.
pub(crate) struct Listener {
    id: f64,
    internals: JsValue,
    _cb: Closure<dyn FnMut(JsValue)>,
}

impl Listener {
    /// The channel argument the command takes.
    fn argument(&self) -> String {
        format!("__CHANNEL__:{}", self.id as u64)
    }

    /// No more events: Tauri finds no callback of this id from now on.
    pub(crate) fn stop(&self) {
        unregister(&self.internals, self.id);
    }
}

fn unregister(internals: &JsValue, id: f64) {
    if let Ok(f) = js_sys::Reflect::get(internals, &JsValue::from_str("unregisterCallback")) {
        if let Ok(f) = f.dyn_into::<js_sys::Function>() {
            let _ = f.call1(internals, &JsValue::from_f64(id));
        }
    }
}

/// Run `machine_apply` for the plan whose hash is `hash`: `on_event` gets
/// each event in order while it runs, `listener` is filled with the channel's
/// callback before the command is called (so the caller can stop it), and
/// the answer is the command's. `Err` is the command's own (lmgw is still
/// starting, the invoke handler refused this document) or a bridge that is
/// not there.
pub(crate) async fn apply(
    hash: &str,
    on_event: impl Fn(MachineApplyEvent) + 'static,
    listener: impl FnOnce(Listener),
) -> Result<MachineApplyResult, String> {
    let (internals, invoke) =
        crate::ui_scale::tauri_invoke().ok_or_else(|| "not inside the lmgw app".to_string())?;
    let register = js_sys::Reflect::get(&internals, &JsValue::from_str("transformCallback"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .ok_or_else(|| "the app's window has no channel bridge".to_string())?;
    let mut order = Ordered::default();
    let id_cell = std::rc::Rc::new(std::cell::Cell::new(None::<f64>));
    let id_seen = id_cell.clone();
    let unreg_at = internals.clone();
    let cb = Closure::<dyn FnMut(JsValue)>::new(move |data: JsValue| {
        let text = js_sys::JSON::stringify(&data)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_default();
        let Ok(frame) = serde_json::from_str::<Frame>(&text) else {
            return;
        };
        if frame.end {
            order.end(frame.index);
        } else if let Some(m) = frame.message {
            for m in order.message(frame.index, m) {
                // An event this page does not know (a newer shell) is left
                // out rather than taken for an error.
                if let Ok(ev) = serde_json::from_value::<MachineApplyEvent>(m) {
                    on_event(ev);
                }
            }
        }
        if order.done() {
            if let Some(id) = id_seen.get() {
                unregister(&unreg_at, id);
            }
        }
    });
    let id = register
        .call2(&internals, cb.as_ref(), &JsValue::FALSE)
        .map_err(|e| js_text(&e))?
        .as_f64()
        .ok_or_else(|| "the channel bridge gave no id".to_string())?;
    id_cell.set(Some(id));
    let held = Listener {
        id,
        internals: internals.clone(),
        _cb: cb,
    };
    let channel = held.argument();
    listener(held);
    let args = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&args, &"hash".into(), &JsValue::from_str(hash));
    let _ = js_sys::Reflect::set(&args, &"onEvent".into(), &JsValue::from_str(&channel));
    let pending = invoke
        .call2(&internals, &JsValue::from_str("machine_apply"), &args)
        .map_err(|e| js_text(&e))?;
    let promise = pending
        .dyn_into::<js_sys::Promise>()
        .map_err(|_| "machine_apply: the shell answered no promise".to_string())?;
    let answer = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(|e| js_text(&e))?;
    let text = js_sys::JSON::stringify(&answer)
        .ok()
        .and_then(|s| s.as_string())
        .unwrap_or_else(|| "null".into());
    serde_json::from_str(&text).map_err(|e| format!("machine_apply: unexpected answer: {e}"))
}

/// A rejection as text: a command's `Err(String)` arrives as the string.
fn js_text(v: &JsValue) -> String {
    if let Some(s) = v.as_string() {
        return s;
    }
    if let Some(e) = v.dyn_ref::<js_sys::Error>() {
        return String::from(e.message());
    }
    js_sys::JSON::stringify(v)
        .ok()
        .and_then(|s| s.as_string())
        .unwrap_or_else(|| "unknown error".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_are_handed_on_in_index_order() {
        let mut o = Ordered::default();
        assert_eq!(o.message(1, json!("b")), Vec::<Value>::new());
        assert_eq!(o.message(0, json!("a")), vec![json!("a"), json!("b")]);
        assert_eq!(o.message(2, json!("c")), vec![json!("c")]);
        // A repeat of one already handed on is not handed on again.
        assert!(o.message(1, json!("b")).is_empty());
        assert!(!o.done());
        o.end(3);
        assert!(o.done());
    }

    #[test]
    fn the_end_waits_for_the_messages_before_it() {
        let mut o = Ordered::default();
        o.end(2);
        assert!(!o.done());
        o.message(1, json!(1));
        assert!(!o.done());
        o.message(0, json!(0));
        assert!(o.done());
    }

    #[test]
    fn frames_read_as_tauri_sends_them() {
        let f: Frame =
            serde_json::from_str(r#"{"message":{"event":"password"},"index":0}"#).unwrap();
        assert_eq!(f.index, 0);
        assert!(!f.end);
        let ev: MachineApplyEvent = serde_json::from_value(f.message.unwrap()).unwrap();
        assert_eq!(ev, MachineApplyEvent::Password);
        let end: Frame = serde_json::from_str(r#"{"end":true,"index":7}"#).unwrap();
        assert!(end.end);
        assert_eq!(end.index, 7);
    }
}
