//! The bridge to the Tauri shell's audio commands (chat-voice §12.3, §13.2).
//!
//! WebKitGTK cannot choose an output, so in the app the shell lists
//! PipeWire's sinks (`audio_outputs`) and points lmgw's own playback streams
//! at one (`audio_output_set`). The page reaches them through the same
//! `__TAURI_INTERNALS__.invoke` the zoom and the file picker use
//! ([`crate::ui_scale::tauri_invoke`]). In a plain browser there is no shell:
//! [`in_shell`] is false and the calls answer an error instead of failing the
//! page.

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use wasm_bindgen::{JsCast, JsValue};

/// `audio_outputs`' answer.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct Outputs {
    pub outputs: Vec<Output>,
    /// Why only the system default is offered ("pipewire-utils not found").
    pub note: Option<String>,
}

/// One PipeWire sink.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct Output {
    /// `node.name`: stable, what the page stores as the device id.
    pub name: String,
    pub description: String,
    pub serial: u64,
    /// `default.audio.sink`.
    pub default: bool,
}

/// `audio_output_set`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct Routed {
    /// The lmgw streams that were moved.
    pub streams: Vec<u32>,
    /// `None`: they follow the system default again.
    pub sink: Option<String>,
    pub serial: Option<u64>,
}

/// Is the page inside the app's window?
pub(crate) fn in_shell() -> bool {
    crate::ui_scale::tauri_invoke().is_some()
}

/// List the outputs PipeWire offers.
pub(crate) async fn audio_outputs() -> Result<Outputs, String> {
    invoke("audio_outputs", json!({})).await
}

/// Point lmgw's playback streams at `sink` (a `node.name`), or with `None`
/// let them follow the system default. The shell waits up to 2 s for a
/// stream to appear, then fails with a message that says so.
pub(crate) async fn audio_output_set(sink: Option<&str>) -> Result<Routed, String> {
    invoke("audio_output_set", json!({ "sink": sink })).await
}

/// One shell command: its answer deserialized, or the rejection's message.
async fn invoke<T: DeserializeOwned>(cmd: &str, args: Value) -> Result<T, String> {
    let (internals, invoke) =
        crate::ui_scale::tauri_invoke().ok_or_else(|| "not inside the lmgw app".to_string())?;
    let args = js_sys::JSON::parse(&args.to_string()).map_err(|e| js_text(&e))?;
    let pending = invoke
        .call2(&internals, &JsValue::from_str(cmd), &args)
        .map_err(|e| js_text(&e))?;
    let promise = pending
        .dyn_into::<js_sys::Promise>()
        .map_err(|_| format!("{cmd}: the shell answered no promise"))?;
    let answer = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(|e| js_text(&e))?;
    let text = js_sys::JSON::stringify(&answer)
        .ok()
        .and_then(|s| s.as_string())
        .unwrap_or_else(|| "null".into());
    serde_json::from_str(&text).map_err(|e| format!("{cmd}: unexpected answer: {e}"))
}

/// A rejection or exception as text: a command's `Err(String)` arrives as
/// the string itself.
pub(crate) fn js_text(v: &JsValue) -> String {
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

    #[test]
    fn the_shells_answers_are_read_as_it_sends_them() {
        let o: Outputs = serde_json::from_value(json!({
            "outputs": [
                {"name": "alsa_output.usb", "description": "Headphones", "serial": 77, "default": false},
                {"name": "alsa_output.pci", "description": "Speakers", "serial": 41, "default": true}
            ],
            "note": null
        }))
        .unwrap();
        assert_eq!(o.outputs.len(), 2);
        assert!(o.outputs[1].default);
        assert_eq!(o.outputs[0].serial, 77);
        let none: Outputs = serde_json::from_value(json!({
            "outputs": [], "note": "system default only: pipewire-utils not found"
        }))
        .unwrap();
        assert!(none.note.unwrap().contains("pipewire-utils"));
        let r: Routed =
            serde_json::from_value(json!({"streams": [12, 13], "sink": null, "serial": null}))
                .unwrap();
        assert_eq!(r.streams, vec![12, 13]);
        assert_eq!(r.sink, None);
    }
}
