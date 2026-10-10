//! Temporary chats (chat-complete §7): the sidebar group, the banner in an
//! open one, Keep, and the silent discard when one is left.
//!
//! A temporary chat lives in the gateway's memory only. Everything else about
//! it — messages, attachments, the settings — goes through the same routes as
//! a stored chat; the negative id is what tells the server which it is.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::chat::Ack;
use lmgw_api_types::chat_threads::ThreadKept;
use serde_json::json;

use super::chat::{matches_query, thread_row, ChatThread, ListItem};
use crate::scope::Scope;
use crate::widgets::Toasts;

/// The sidebar's "Temporary" group: every temporary chat the gateway holds
/// (the open one, and any that survived a reload) that matches the filter.
pub(super) fn temp_items(temps: &[ChatThread], words: &[String], today: &str) -> Vec<ListItem> {
    let shown: Vec<&ChatThread> = temps.iter().filter(|t| matches_query(t, words)).collect();
    if shown.is_empty() {
        return Vec::new();
    }
    let mut out = vec![ListItem::Head("Temporary".to_string(), shown.len())];
    out.extend(shown.into_iter().map(|t| thread_row(t, today)));
    out
}

/// Throw a temporary chat away: no question, no toast. It is gone from the
/// gateway's memory either way, and a 404 only means it already was.
pub(super) fn discard(id: i64) {
    debug_assert!(id < 0, "only temporary chats are discarded silently");
    spawn_local(async move {
        let _ =
            crate::api::post::<Ack, _>(format!("/chat/api/threads/{id}/delete"), &json!({})).await;
    });
}

/// **Keep**: write the chat to the database and open it under its new id.
/// The old id and every message id in it are gone, so the stored copy is
/// reopened rather than patched. `busy` is set while the write is out.
pub(super) fn keep(
    id: i64,
    current: RwSignal<Option<ChatThread>>,
    busy: RwSignal<bool>,
    scope: Scope,
    toasts: Toasts,
    open: Callback<i64>,
    refresh: Callback<()>,
) {
    if busy.get_untracked() {
        return;
    }
    busy.set(true);
    scope.spawn(async move {
        let res = crate::api::post::<ThreadKept, _>(
            format!("/chat/api/threads/{id}/persist"),
            &json!({}),
        )
        .await;
        busy.set(false);
        match res {
            Ok(k) => {
                // Gone from memory now: opening the stored copy must not
                // try to discard it.
                current.update(|c| {
                    if let Some(t) = c.as_mut().filter(|t| t.id == id) {
                        t.temporary = false;
                    }
                });
                open.run(k.id);
                refresh.run(());
                toasts.ok("chat kept");
            }
            // The voice session's gateway side closes after its journal
            // drained; a Keep that came first is refused for it (§8.1).
            Err(crate::api::Error::Api(e)) if e.code == "voice_session_active" => {
                toasts.err("the voice session is still closing: try Keep again in a moment")
            }
            Err(e) => toasts.err(format!("keeping the chat failed: {e}")),
        }
    });
}

/// The strip above the composer of an open temporary chat.
#[component]
pub(super) fn TempBanner(
    /// The open temporary chat, for its Export menu.
    #[prop(into)]
    thread: Signal<Option<i64>>,
    /// A reply is streaming in it: Keep waits.
    #[prop(into)]
    streaming: Signal<bool>,
    /// Voice mode is on: Keep waits until it ends (the gateway refuses it
    /// while the thread is bound, chat-voice §8.1).
    #[prop(into)]
    voice_mode: Signal<bool>,
    /// Voice mode was left and the gateway's side of the session has not
    /// closed yet: its journal still drains, and the thread is still bound
    /// (WP9 review NIT 9).
    #[prop(into)]
    voice_closing: Signal<bool>,
    busy: RwSignal<bool>,
    on_keep: Callback<()>,
) -> impl IntoView {
    view! {
        <div class="temp-strip" role="status">
            <span>"Temporary — not saved. Discarded when you leave it or lmgw restarts."</span>
            <button
                type="button"
                class="btn ghost sm"
                title=move || {
                    if streaming.get() {
                        "wait for the reply to finish".to_string()
                    } else if voice_mode.get() {
                        "leave voice mode first: a chat in voice mode is kept once the session ends"
                            .to_string()
                    } else if voice_closing.get() {
                        "the voice session is still closing: Keep in a moment".to_string()
                    } else {
                        "Save this chat as an ordinary conversation".to_string()
                    }
                }
                data-keep=""
                disabled=move || {
                    busy.get() || streaming.get() || voice_mode.get() || voice_closing.get()
                }
                on:click=move |_| on_keep.run(())
            >
                "Keep"
            </button>
            <super::chat_export::ExportMenu thread=thread/>
        </div>
    }
}
