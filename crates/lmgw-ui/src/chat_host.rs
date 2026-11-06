//! The dashboard as the Chat's host (lmgw-chat-ui; chat-plane design, "WP3b:
//! the lmgw-chat-ui crate"): the `/chat` route reads its query, keeps
//! `/chat?t=<id>` in the address bar, links to the dashboard's own pages,
//! hands the Chat the shell's stream as its live updates (D1), the
//! realtime socket of the page's own origin and the machine check's
//! surfaces (machine-check design D177), and serves the Chat's assets from
//! the root. The transport is `fetch` (nothing to provide) and the preview
//! the sandboxed `srcdoc` frame, except in a debug build's test host
//! (`crate::test_host`).

use std::sync::Arc;

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;
use lmgw_api_types::chat_feed::FeedHold;
use lmgw_api_types::VramStatus;
use lmgw_chat_ui::{
    provide_chat_host, ChatAddress, ChatHost, ChatLive, ChatLocation, ChatVoice, PreviewFrame,
};
use wasm_bindgen::JsValue;

/// The dashboard's address bar.
struct Dashboard {
    opened: ChatAddress,
}

impl ChatLocation for Dashboard {
    fn opened_at(&self) -> ChatAddress {
        self.opened.clone()
    }

    /// Keep the address bar on `/chat?t=<id>` (bare `/chat` with nothing
    /// open), so a reload or a copied link returns to the same thread. Raw
    /// `replaceState` rather than the router's `navigate`: this fires on
    /// every thread switch, including mid-stream, and must not re-run route
    /// matching — the router picks the location back up on `popstate`.
    fn show_thread(&self, id: Option<i64>) {
        let url = match id {
            Some(id) => format!("/chat?t={id}"),
            None => "/chat".to_string(),
        };
        if let Some(h) = web_sys::window().and_then(|w| w.history().ok()) {
            let _ = h.replace_state_with_url(&JsValue::NULL, "", Some(&url));
        }
    }

    /// The dashboard's pages are its own routes.
    fn dashboard_href(&self, path: &str) -> Option<String> {
        Some(path.to_string())
    }
}

/// What `/chat?t=…&m=…&folder=…&settings=1` asks for.
fn address(
    t: Option<String>,
    m: Option<String>,
    folder: Option<String>,
    settings: Option<String>,
) -> ChatAddress {
    let id = |v: Option<String>| v.and_then(|v| v.parse::<i64>().ok());
    ChatAddress {
        thread: id(t),
        message: id(m),
        folder_settings: (settings.as_deref() == Some("1"))
            .then(|| id(folder))
            .flatten(),
    }
}

/// The shell's `vram` frame's hold, in the feed's words.
fn shell_hold(v: &VramStatus) -> FeedHold {
    FeedHold {
        active: v.hold_active,
        benchmark: v.benchmark.is_some(),
    }
}

/// The `/chat` route: the Chat with the dashboard's seams.
#[component]
pub fn ChatRoute() -> impl IntoView {
    // Read untracked: the Chat rewrites the address on every switch, and a
    // query read again would loop.
    let opened = use_query_map()
        .with_untracked(|q| address(q.get("t"), q.get("m"), q.get("folder"), q.get("settings")));
    let mut host = ChatHost::new(
        Arc::new(Dashboard { opened }),
        crate::session::locked_signal(),
    );
    host.voice = Some(ChatVoice {
        realtime_url: Arc::new(|tid| {
            let loc = window().location();
            lmgw_client::requests::realtime_page_url(
                &loc.protocol().unwrap_or_default(),
                &loc.host().unwrap_or_default(),
                tid,
            )
        }),
    });
    match crate::test_host::preview_page() {
        // The test host hands the Chat no stream of the shell's: it follows
        // its own feed through the relaying transport, and previews in the
        // check's page. It hands no machine check either, as a paired
        // device's sidebar (machine-check design D178).
        Some(src) => host.preview = PreviewFrame::Posted { src },
        None => {
            let live = crate::live::use_live();
            host.live = ChatLive::Host {
                changes: live.chat,
                hold: Signal::derive(move || live.vram.with(|v| v.as_ref().map(shell_hold))),
            };
            // The banner, the card above the empty state and the fix under
            // a `gpu_setup` card (machine-check design D177).
            host.machine = Some(crate::widgets::machine_check::chat_slot());
        }
    }
    provide_chat_host(host);
    view! { <lmgw_chat_ui::Chat/> }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::VramBenchmark;

    #[test]
    fn the_shell_s_hold_is_the_vram_frame_s() {
        let v = VramStatus {
            hold_active: true,
            ..Default::default()
        };
        assert_eq!(
            shell_hold(&v),
            FeedHold {
                active: true,
                benchmark: false
            }
        );
        let v = VramStatus {
            benchmark: Some(VramBenchmark::default()),
            ..Default::default()
        };
        assert_eq!(
            shell_hold(&v),
            FeedHold {
                active: false,
                benchmark: true
            }
        );
    }

    #[test]
    fn the_query_is_the_chat_s_address() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            address(s("3"), s("41"), None, None),
            ChatAddress {
                thread: Some(3),
                message: Some(41),
                folder_settings: None
            }
        );
        // A folder link opens the form only with `settings=1`.
        assert_eq!(address(None, None, s("7"), None).folder_settings, None);
        assert_eq!(address(None, None, s("7"), s("1")).folder_settings, Some(7));
        assert_eq!(address(s("x"), None, None, None).thread, None);
    }
}
