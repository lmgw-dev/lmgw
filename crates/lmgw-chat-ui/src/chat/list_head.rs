//! The thread list's header bar actions (the modern design's list head,
//! docs/design/ui-rebuild/plan.md "Chat"): New chat as the header's compose
//! button, and a menu with the other ways to start (an Admin chat, a
//! temporary chat, a folder) and the export of every chat. The list's
//! title, count and fold button are the split pane's own head.

use leptos::prelude::*;
use lmgw_ui_kit::widgets::{Ico, Icon, MenuItem, RowMenu};

use crate::chat_folders::FolderEnv;

/// New chat and the list's menu, for the panel head's actions.
#[component]
pub(crate) fn ListActions(
    /// What New chat says it does ("New chat in Kai").
    #[prop(into)]
    new_label: Signal<String>,
    on_new: Callback<&'static str>,
    folders: FolderEnv,
) -> impl IntoView {
    let export_all = RwSignal::new(false);
    // The titles are the ones the buttons these entries replace had: what
    // each makes, in a sentence.
    let items = Signal::derive(move || {
        vec![
            MenuItem::new("New admin chat", move || on_new.run("admin"))
                .title("New Admin Chat — wired to the lmgw self-admin tools"),
            MenuItem::new("New temporary chat", move || on_new.run("temporary"))
                .title("New temporary chat — kept in memory only, discarded when you leave it"),
            MenuItem::new("New folder", move || folders.create()).title(
                "New folder — group conversations, with settings new chats in it start from",
            ),
            MenuItem::new("Export all chats…", move || export_all.set(true))
                .title("Download every chat as a .zip"),
        ]
    });
    view! {
        <button
            type="button"
            class="icon-btn sm chat-list-new"
            title=move || new_label.get()
            aria-label=move || new_label.get()
            on:click=move |_| on_new.run("chat")
        >
            <Icon ico=Ico::Compose/>
        </button>
        <span class="chat-list-more">
            <RowMenu items=items title="More: an Admin, temporary chat, a folder, export" icon=Ico::More/>
        </span>
        <crate::chat_export::ExportAll open=export_all/>
    }
}
