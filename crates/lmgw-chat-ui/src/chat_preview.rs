//! The HTML preview's frame, as the Chat's host hands it (chat-plane design,
//! "WP3a: the transport seam"; kai-firefox design §8.2 item 6).
//!
//! The dashboard's is a `srcdoc` frame with a `sandbox` attribute: an opaque
//! origin that may run the previewed page's scripts. A `srcdoc` frame
//! inherits its parent's Content-Security-Policy, so inside an extension page
//! (`script-src 'self'`) the previewed page's scripts would be refused. Such
//! a host hands the Chat [`PreviewFrame::Posted`]: a page of its own, loaded
//! as the frame's `src` without a `sandbox` attribute (its own policy
//! sandboxes it), which receives the HTML by `postMessage` once it has
//! loaded ([`message`]) and shows it as it likes. The code goes only to that
//! first load: a frame that has navigated since (the previewed page followed
//! a link) holds another document, which gets nothing ([`may_post`]).

use leptos::html::Iframe;
use leptos::prelude::*;
use serde_json::{json, Value};

/// Where the preview runs: what the host hands the Chat
/// ([`crate::ChatHost::preview`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PreviewFrame {
    /// A sandboxed `srcdoc` frame (the dashboard's).
    #[default]
    Srcdoc,
    /// The host's page at `src`, handed the HTML by `postMessage`.
    Posted { src: String },
}

/// The message a [`PreviewFrame::Posted`] page receives once it has loaded:
/// `{type: "lmgw-preview", code, lang}`, `lang` as the code block names it
/// (`html`, `svg`, `xhtml`; empty for none).
pub(super) fn message(code: &str, lang: &str) -> Value {
    json!({ "type": "lmgw-preview", "code": code, "lang": lang })
}

/// Whether the frame's document may be posted the code: only while the
/// host's page is the frame's first and only load. A second load is a
/// navigation away from it (the previewed page followed a link, or replaced
/// the host's page), and what is there now is not the host's.
pub(super) fn may_post(loads: u32) -> bool {
    loads == 1
}

/// The preview of `code`, in the frame the host handed the page. Mounted
/// only while the preview is open: an `<iframe>` created inside a closed
/// `<dialog>` never gets a browsing context (Chromium/WebKit).
#[component]
pub(super) fn PreviewPane(
    #[prop(into)] code: Signal<String>,
    #[prop(into)] lang: Signal<String>,
) -> impl IntoView {
    match crate::host::use_chat_host().preview {
        PreviewFrame::Srcdoc => view! {
            <iframe
                class="preview-frame"
                sandbox="allow-scripts allow-modals allow-forms allow-popups allow-pointer-lock"
                srcdoc=move || code.get()
            ></iframe>
        }
        .into_any(),
        PreviewFrame::Posted { src } => {
            let frame = NodeRef::<Iframe>::new();
            // The frame's loads: the first is the host's page.
            let loads = StoredValue::new(0u32);
            // The page is the host's; the code goes to it once it loaded, and
            // again whenever the open preview's code changes, as long as it
            // is still the host's page.
            let post = move || {
                if !may_post(loads.get_value()) {
                    return;
                }
                let Some(win) = frame.get_untracked().and_then(|f| f.content_window()) else {
                    return;
                };
                let text = message(&code.get_untracked(), &lang.get_untracked()).to_string();
                if let Ok(msg) = js_sys::JSON::parse(&text) {
                    // The page's origin may be opaque (a sandboxed page), which
                    // no target origin can name; what goes is the code block
                    // the page shows anyway.
                    let _ = win.post_message(&msg, "*");
                }
            };
            Effect::new(move |prev: Option<()>| {
                code.track();
                if prev.is_some() {
                    post();
                }
            });
            view! {
                <iframe
                    class="preview-frame"
                    src=src
                    node_ref=frame
                    on:load=move |_| {
                        loads.update_value(|n| *n = n.saturating_add(1));
                        post();
                    }
                ></iframe>
            }
            .into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_posted_page_gets_the_code_and_its_language() {
        let m = message("<p onclick=\"x()\">hi</p>", "html");
        assert_eq!(m["type"], "lmgw-preview");
        assert_eq!(m["code"], "<p onclick=\"x()\">hi</p>");
        assert_eq!(m["lang"], "html");
        assert_eq!(PreviewFrame::default(), PreviewFrame::Srcdoc);
    }

    #[test]
    fn only_the_host_s_first_load_is_posted_to() {
        assert!(!may_post(0), "not loaded yet");
        assert!(may_post(1));
        assert!(!may_post(2), "navigated away: another document");
        assert!(!may_post(7));
    }
}
