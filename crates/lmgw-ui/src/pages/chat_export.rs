//! Export in the Chat UI (chat-complete §6): the entries that download a
//! thread (Markdown or lossless JSON), a folder or every chat as a `.zip`.
//!
//! Every entry is the shared `download(href, name)` click on a `<a download>`:
//! the routes answer with `Content-Disposition: attachment`, so a browser
//! saves the file under the server's name. In the Tauri window the app's
//! `on_download` handler saves it to the Downloads folder and tells the page
//! (`lmgw-downloaded`, toasted by `ToastHost`).

use leptos::prelude::*;

use super::docs::download;
use crate::widgets::{MenuItem, Modal, ModalFooter, RowMenu, Select};

/// The route of one thread's export. `format` is `md` or `json`.
pub(super) fn thread_href(id: i64, format: &str) -> String {
    format!("/chat/api/threads/{id}/export?format={format}")
}

/// The route of a folder's zip; `archived` is `0`, `1` or `all`.
pub(super) fn folder_href(id: i64, format: &str, archived: &str) -> String {
    format!("/chat/api/folders/{id}/export?format={format}&archived={archived}")
}

/// The route of the zip of every stored chat.
pub(super) fn all_href(format: &str, archived: &str) -> String {
    format!("/chat/api/export?format={format}&archived={archived}")
}

fn ext(format: &str) -> &str {
    if format == "json" {
        "json"
    } else {
        "md"
    }
}

/// Download one thread. The name is a fallback: the server's
/// `Content-Disposition` names the file.
pub(super) fn download_thread(id: i64, format: &str) {
    download(
        &thread_href(id, format),
        &format!("lmgw-chat-{}.{}", id.abs(), ext(format)),
    );
}

/// The two menu entries that export thread `id`.
pub(super) fn thread_items(id: i64) -> Vec<MenuItem> {
    vec![
        MenuItem::new("Export as Markdown", move || download_thread(id, "md")),
        MenuItem::new("Export as JSON", move || download_thread(id, "json")),
    ]
}

/// The two menu entries that export a folder's chats as a zip (active and
/// archived).
pub(super) fn folder_items(id: i64) -> Vec<MenuItem> {
    ["md", "json"]
        .into_iter()
        .map(|f| {
            MenuItem::new(
                if f == "md" {
                    "Export folder (Markdown, zip)"
                } else {
                    "Export folder (JSON, zip)"
                },
                move || download(&folder_href(id, f, "all"), "lmgw-folder.zip"),
            )
        })
        .collect()
}

/// An "Export" menu for the open thread: in the chat head, and in the
/// temporary chat's banner (its only way out besides Keep).
#[component]
pub(super) fn ExportMenu(#[prop(into)] thread: Signal<Option<i64>>) -> impl IntoView {
    let items = Signal::derive(move || thread.get().map(thread_items).unwrap_or_default());
    view! {
        <span class="export-menu">
            <RowMenu
                items=items
                title="Download this chat"
                label=Signal::derive(|| "Export".to_string())
            />
        </span>
    }
}

/// "Export all chats…" in the sidebar: a ⋯ menu whose one entry opens the
/// choice of format and which chats (active, archived, both).
#[component]
pub(super) fn ExportAll() -> impl IntoView {
    let open = RwSignal::new(false);
    let format = RwSignal::new("md".to_string());
    let archived = RwSignal::new("all".to_string());
    let items =
        Signal::derive(move || vec![MenuItem::new("Export all chats…", move || open.set(true))]);
    let go = move |_| {
        let f = format.get_untracked();
        download(&all_href(&f, &archived.get_untracked()), "lmgw-chats.zip");
        open.set(false);
    };
    view! {
        <span class="export-all">
            <RowMenu items=items title="More: export every chat"/>
        </span>
        <Modal open=open title="Export all chats">
            <div class="field">
                <label>"Format"</label>
                <Select
                    value=format
                    options=Signal::derive(|| {
                        vec![
                            ("md".to_string(), "Markdown — readable transcripts".to_string()),
                            ("json".to_string(), "JSON — lossless, with attachment bytes".to_string()),
                        ]
                    })
                />
            </div>
            <div class="field">
                <label>"Which chats"</label>
                <Select
                    value=archived
                    options=Signal::derive(|| {
                        vec![
                            ("all".to_string(), "Active and archived".to_string()),
                            ("0".to_string(), "Active only".to_string()),
                            ("1".to_string(), "Archived only".to_string()),
                        ]
                    })
                />
            </div>
            <p class="muted">
                "One file per thread in a .zip, folders as directories. Temporary chats are not included: export one from its own banner."
            </p>
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| open.set(false)>
                    "Cancel"
                </button>
                <button class="btn primary" on:click=go>
                    "Download"
                </button>
            </ModalFooter>
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hrefs_name_the_routes_the_server_serves() {
        assert_eq!(thread_href(7, "md"), "/chat/api/threads/7/export?format=md");
        assert_eq!(
            thread_href(-3, "json"),
            "/chat/api/threads/-3/export?format=json"
        );
        assert_eq!(
            folder_href(2, "json", "all"),
            "/chat/api/folders/2/export?format=json&archived=all"
        );
        assert_eq!(all_href("md", "1"), "/chat/api/export?format=md&archived=1");
    }
}
