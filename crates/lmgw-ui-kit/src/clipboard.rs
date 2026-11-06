//! The browser's clipboard, written once for every copy that says whether
//! it happened: the page may have no clipboard at all (one opened over
//! plain http from another computer is not a secure context), and a write
//! is a promise the browser can refuse (no focus, no permission).

use leptos::prelude::window;
use wasm_bindgen::{JsCast, JsValue};

/// Why a page has no clipboard: the browser gives it only in a secure
/// context.
pub const NOT_SECURE: &str = "this browser lets a page copy only over a secure connection \
                              (https, or this computer's own address)";

/// The clipboard, when the browser lets the page have it.
pub fn clipboard() -> Option<web_sys::Clipboard> {
    let nav = window().navigator();
    js_sys::Reflect::get(&nav, &"clipboard".into())
        .ok()
        .filter(|c| !c.is_undefined() && !c.is_null())
        .and_then(|c| c.dyn_into::<web_sys::Clipboard>().ok())
}

/// A refused write's words, as the browser gave them, without their full
/// stop: every caller puts the reason inside a sentence of its own.
pub fn refusal(e: &JsValue) -> String {
    let words = e
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(e, &"message".into())
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| "the browser refused it".into());
    without_stop(&words)
}

/// `words` without the full stops that end it, so "Not copied: {why}." ends
/// in one (Chrome's refusal ends in its own: "… Document is not focused.").
fn without_stop(words: &str) -> String {
    words.trim_end().trim_end_matches('.').to_string()
}

/// Write `text` to the clipboard: `Ok` once the browser took it, else why
/// not ([`NOT_SECURE`], or the browser's refusal).
pub async fn write_text(text: &str) -> Result<(), String> {
    let cb = clipboard().ok_or_else(|| NOT_SECURE.to_string())?;
    wasm_bindgen_futures::JsFuture::from(cb.write_text(text))
        .await
        .map(|_| ())
        .map_err(|e| refusal(&e))
}

#[cfg(test)]
mod tests {
    use super::without_stop;

    /// Chrome's own words for a page without focus, as WP8's live round met
    /// them in a headless window (find-a-model design D111).
    #[test]
    fn a_refusal_loses_its_full_stop() {
        let chrome = "Failed to execute 'writeText' on 'Clipboard': Document is not focused.";
        assert_eq!(
            format!("Not copied: {}.", without_stop(chrome)),
            "Not copied: Failed to execute 'writeText' on 'Clipboard': Document is not focused."
        );
        assert_eq!(
            without_stop("the browser refused it"),
            "the browser refused it"
        );
        assert_eq!(without_stop("denied. "), "denied");
    }
}
