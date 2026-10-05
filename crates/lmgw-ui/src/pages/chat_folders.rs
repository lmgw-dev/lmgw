//! Chat folders (chat-complete §5): the sidebar's Folders block, moving a
//! thread into one (drag and drop, or the row menu's "Move to…"), and the
//! dialogs — Folder settings, Delete, Move.
//!
//! One level. A thread is listed exactly once: in its folder when it has one
//! (pinned first, then by last activity), otherwise in Pinned or under its
//! date. A folder's *defaults* are what a new chat in it starts from — a copy;
//! the form for them is the thread settings' own fields
//! ([`super::chat_settings`]), so a setting added there arrives here too.
//!
//! Export of a folder is added by a later package: its menu item goes in
//! [`FolderHeader`]'s menu, beside Delete.

use std::collections::HashSet;

use leptos::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};

use super::chat::{matches_query_in, ChatThread, ListItem, SettingsDraft};
use super::chat_settings::{draft_patch, DraftErrors, SettingsFields};
use crate::scope::Scope;
use crate::widgets::{
    use_toasts, ConfirmButton, MenuItem, Modal, ModalFooter, ModelPicker, RowMenu, Toasts,
};

/// One folder as the thread list carries it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(super) struct FolderInfo {
    pub id: i64,
    pub name: String,
    pub sort: i64,
    /// The `ThreadDefaults` object, kept as JSON: the form reads it through
    /// [`thread_from_defaults`], so a field added on the server needs no
    /// mirror struct here.
    pub defaults: Value,
    pub threads_active: i64,
    pub threads_archived: i64,
}

/// A folder's header line in the list.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct FolderRow {
    pub info: FolderInfo,
    /// Threads listed under it now (the matches, while filtering).
    pub shown: usize,
    pub collapsed: bool,
}

impl FolderRow {
    pub(super) fn key(&self) -> String {
        format!(
            "f:{}:{}:{}:{}",
            self.info.id, self.info.name, self.shown, self.collapsed
        )
    }
}

/// The name of the folder `t` sits in, `None` when it has none — or one the
/// list does not know (it is then listed like an unfiled thread).
pub(super) fn folder_name<'a>(t: &ChatThread, folders: &'a [FolderInfo]) -> Option<&'a str> {
    let id = t.folder_id?;
    folders.iter().find(|f| f.id == id).map(|f| f.name.as_str())
}

/// The Folders block: every folder in the server's order, each followed by
/// its threads unless collapsed. While filtering, a folder shows when it or
/// one of its threads matches (its name counts as part of every thread's
/// text), and is kept open so the matches are visible. Empty folders are
/// listed too: they are somewhere to drop a thread.
pub(super) fn folder_block(
    threads: &[ChatThread],
    folders: &[FolderInfo],
    collapsed: &HashSet<i64>,
    words: &[String],
    row: &dyn Fn(&ChatThread) -> ListItem,
) -> Vec<ListItem> {
    let filtering = !words.is_empty();
    let mut out = Vec::new();
    for f in folders {
        let mut inside: Vec<&ChatThread> = threads
            .iter()
            .filter(|t| t.folder_id == Some(f.id))
            .collect();
        // Pinned first, then most recently active — stable, so the server's
        // own order decides ties.
        inside.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        let matching: Vec<&ChatThread> = inside
            .into_iter()
            .filter(|t| matches_query_in(t, Some(&f.name), words))
            .collect();
        let name_hits = filtering && {
            let name = f.name.to_lowercase();
            words.iter().all(|w| name.contains(w.as_str()))
        };
        if filtering && matching.is_empty() && !name_hits {
            continue;
        }
        let closed = collapsed.contains(&f.id) && !filtering;
        out.push(ListItem::Folder(FolderRow {
            info: f.clone(),
            shown: matching.len(),
            collapsed: closed,
        }));
        if !closed {
            out.extend(matching.into_iter().map(|t| row(t).nested()));
        }
    }
    out
}

/// A folder's defaults as a thread, so the shared settings form can be seeded
/// with them. `null` keys are unset, which a thread spells with its own
/// defaults.
pub(super) fn thread_from_defaults(defaults: &Value) -> ChatThread {
    let mut v = defaults.clone();
    if let Some(o) = v.as_object_mut() {
        o.retain(|_, x| !x.is_null());
    }
    serde_json::from_value(v).unwrap_or_default()
}

/// The `defaults` a folder-settings save sends: the settings form's patch,
/// with what the folder should leave to the global behaviour left `null` — a
/// blank prompt (rather than an empty one that would blank every new chat's
/// prompt), no tool servers and a voice that sets nothing.
pub(super) fn defaults_body(patch: Value, model: &str) -> Value {
    let mut d = patch;
    if let Some(o) = d.as_object_mut() {
        if o.get("system_prompt").and_then(Value::as_str) == Some("") {
            o.insert("system_prompt".into(), Value::Null);
        }
        if o.get("mcp_tools")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            o.insert("mcp_tools".into(), Value::Null);
        }
        // A voice that sets nothing is no default (chat-voice §2.2).
        if o.get("voice")
            .and_then(Value::as_object)
            .is_some_and(serde_json::Map::is_empty)
        {
            o.insert("voice".into(), Value::Null);
        }
        o.insert(
            "model_alias".into(),
            if model.is_empty() {
                Value::Null
            } else {
                json!(model)
            },
        );
    }
    d
}

fn collapsed_set(stored: &str) -> HashSet<i64> {
    stored
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

/// Everything the folder UI shares with the page.
#[derive(Clone, Copy)]
pub(super) struct FolderEnv {
    pub folders: RwSignal<Vec<FolderInfo>>,
    /// Collapsed folder ids, kept across reloads (localStorage).
    collapsed_text: RwSignal<String>,
    /// The thread being dragged, if any: the drop zones show while it is.
    pub dragged: RwSignal<Option<i64>>,
    /// The folder whose name is being edited in place.
    renaming: RwSignal<Option<i64>>,
    settings_of: RwSignal<Option<i64>>,
    settings_open: RwSignal<bool>,
    delete_of: RwSignal<Option<i64>>,
    delete_open: RwSignal<bool>,
    move_of: RwSignal<Option<i64>>,
    move_open: RwSignal<bool>,
    scope: Scope,
    toasts: Toasts,
    model_sel: RwSignal<String>,
    current: RwSignal<Option<ChatThread>>,
    open: Callback<i64>,
    refresh: Callback<()>,
    /// Delete a thread as the page does (falls through to another one when
    /// it was open).
    delete_thread: Callback<i64>,
}

impl FolderEnv {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        folders: RwSignal<Vec<FolderInfo>>,
        scope: Scope,
        model_sel: RwSignal<String>,
        current: RwSignal<Option<ChatThread>>,
        open: Callback<i64>,
        refresh: Callback<()>,
        delete_thread: Callback<i64>,
    ) -> Self {
        Self {
            folders,
            collapsed_text: crate::prefs::persisted_string("chat.folders.collapsed", ""),
            dragged: RwSignal::new(None),
            renaming: RwSignal::new(None),
            settings_of: RwSignal::new(None),
            settings_open: RwSignal::new(false),
            delete_of: RwSignal::new(None),
            delete_open: RwSignal::new(false),
            move_of: RwSignal::new(None),
            move_open: RwSignal::new(false),
            scope,
            toasts: use_toasts(),
            model_sel,
            current,
            open,
            refresh,
            delete_thread,
        }
    }

    /// The collapsed folders (tracked).
    pub(super) fn collapsed(&self) -> HashSet<i64> {
        collapsed_set(&self.collapsed_text.get())
    }

    fn set_collapsed(&self, id: i64, collapsed: bool) {
        let mut set = collapsed_set(&self.collapsed_text.get_untracked());
        if collapsed {
            set.insert(id);
        } else {
            set.remove(&id);
        }
        let mut ids: Vec<i64> = set.into_iter().collect();
        ids.sort_unstable();
        self.collapsed_text
            .set(ids.iter().map(i64::to_string).collect::<Vec<_>>().join(","));
    }

    fn toggle(&self, id: i64) {
        let now = collapsed_set(&self.collapsed_text.get_untracked()).contains(&id);
        self.set_collapsed(id, !now);
    }

    pub(super) fn folder(&self, id: i64) -> Option<FolderInfo> {
        self.folders
            .with_untracked(|f| f.iter().find(|f| f.id == id).cloned())
    }

    /// "New folder": made at once under a placeholder name and put straight
    /// into rename, rather than asking for the name in a dialog first.
    pub(super) fn create(&self) {
        let env = *self;
        self.scope.spawn(async move {
            match crate::api::post::<FolderInfo, _>(
                "/chat/api/folders",
                &json!({ "name": "New folder" }),
            )
            .await
            {
                Ok(f) => {
                    env.refresh.run(());
                    env.renaming.set(Some(f.id));
                }
                Err(e) => env.toasts.err(format!("creating the folder failed: {e}")),
            }
        });
    }

    /// A new chat that starts from the folder's defaults (the server applies
    /// them), opened at once.
    fn new_chat_in(&self, folder: i64) {
        let env = *self;
        let alias = self.model_sel.get_untracked();
        self.scope.spawn(async move {
            let body = json!({ "model_alias": alias, "kind": "chat", "folder_id": folder });
            match crate::api::post::<ChatThread, _>("/chat/api/threads", &body).await {
                Ok(t) => {
                    env.set_collapsed(folder, false);
                    env.refresh.run(());
                    env.open.run(t.id);
                }
                Err(e) => env
                    .toasts
                    .err(format!("starting a chat in the folder failed: {e}")),
            }
        });
    }

    fn rename(&self, id: i64, name: String) {
        let name = name.trim().to_string();
        let unchanged = self.folder(id).is_none_or(|f| f.name == name);
        self.renaming.set(None);
        if name.is_empty() || unchanged {
            return;
        }
        let env = *self;
        self.scope.spawn(async move {
            match crate::api::post::<Value, _>(
                format!("/chat/api/folders/{id}"),
                &json!({ "name": name }),
            )
            .await
            {
                Ok(_) => env.refresh.run(()),
                Err(e) => env.toasts.err(format!("renaming the folder failed: {e}")),
            }
        });
    }

    /// File `thread` under `folder` (`None`: out of any folder).
    pub(super) fn move_thread(&self, thread: i64, folder: Option<i64>) {
        let env = *self;
        self.scope.spawn(async move {
            match crate::api::post::<Value, _>(
                format!("/chat/api/threads/{thread}/move"),
                &json!({ "folder_id": folder }),
            )
            .await
            {
                Ok(_) => {
                    env.current.try_update(|c| {
                        if let Some(t) = c.as_mut().filter(|t| t.id == thread) {
                            t.folder_id = folder;
                        }
                    });
                    env.refresh.run(());
                }
                Err(e) => env
                    .toasts
                    .err(format!("moving the conversation failed: {e}")),
            }
        });
    }

    fn delete(&self, id: i64, delete_threads: bool) {
        let env = *self;
        self.delete_open.set(false);
        let open_here = self
            .current
            .with_untracked(|c| c.as_ref().is_some_and(|t| t.folder_id == Some(id)));
        let open_id = self.current.with_untracked(|c| c.as_ref().map(|t| t.id));
        self.scope.spawn(async move {
            let body = json!({ "threads": if delete_threads { "delete" } else { "keep" } });
            match crate::api::post::<Value, _>(format!("/chat/api/folders/{id}/delete"), &body)
                .await
            {
                Ok(_) => {
                    env.toasts.ok("folder deleted");
                    // The open chat went with it: the page's own delete falls
                    // through to another one (the server already lost it).
                    match open_id {
                        Some(t) if delete_threads && open_here => env.delete_thread.run(t),
                        Some(t) if open_here => {
                            env.current.try_update(|c| {
                                if let Some(c) = c.as_mut().filter(|c| c.id == t) {
                                    c.folder_id = None;
                                }
                            });
                            env.refresh.run(());
                        }
                        _ => env.refresh.run(()),
                    }
                }
                Err(e) => env.toasts.err(format!("deleting the folder failed: {e}")),
            }
        });
    }

    pub(super) fn open_move(&self, thread: i64) {
        self.move_of.set(Some(thread));
        self.move_open.set(true);
    }
}

/// A folder's header in the list: collapse toggle, name (or its rename box),
/// count, menu; also a drop target for a dragged thread.
#[component]
pub(super) fn FolderHeader(row: FolderRow, env: FolderEnv) -> impl IntoView {
    let id = row.info.id;
    let name = row.info.name.clone();
    let collapsed = row.collapsed;
    let over = RwSignal::new(false);
    let renaming = Memo::new(move |_| env.renaming.get() == Some(id));
    let menu = Signal::derive(move || {
        let mut items = vec![
            MenuItem::new("New chat here", move || env.new_chat_in(id)),
            MenuItem::new("Rename", move || env.renaming.set(Some(id))),
            MenuItem::new("Folder settings", move || {
                env.settings_of.set(Some(id));
                env.settings_open.set(true);
            }),
        ];
        items.extend(super::chat_export::folder_items(id));
        items.push(MenuItem::new("Delete…", move || {
            env.delete_of.set(Some(id));
            env.delete_open.set(true);
        }));
        items
    });
    let rename_box: NodeRef<leptos::html::Input> = NodeRef::new();
    Effect::new(move |_| {
        if renaming.get() {
            if let Some(el) = rename_box.get() {
                let _ = el.focus();
                el.select();
            }
        }
    });
    // Enter or blur commits, Esc cancels — and only one of them counts:
    // taking the box away fires a blur of its own.
    let finished = StoredValue::new(false);
    view! {
        <div
            class="thread-folder"
            class:drop-over=move || over.get()
            role="button"
            tabindex="0"
            aria-expanded=(!collapsed).to_string()
            data-folder-id=id
            title=if collapsed { "Expand" } else { "Collapse" }
            on:click=move |_| {
                if !renaming.get_untracked() {
                    env.toggle(id);
                }
            }
            on:keydown=move |ev| {
                if (ev.key() == "Enter" || ev.key() == " ") && ev.target() == ev.current_target() {
                    ev.prevent_default();
                    env.toggle(id);
                }
            }
            on:dragover=move |ev| {
                if env.dragged.get_untracked().is_some() {
                    ev.prevent_default();
                    over.set(true);
                }
            }
            on:dragleave=move |_| over.set(false)
            on:drop=move |ev| {
                ev.prevent_default();
                over.set(false);
                if let Some(t) = env.dragged.get_untracked() {
                    env.dragged.set(None);
                    env.move_thread(t, Some(id));
                }
            }
        >
            <span class="folder-chev" class:collapsed=collapsed aria-hidden="true">
                "▾"
            </span>
            <Show
                when=move || renaming.get()
                fallback={
                    let name = name.clone();
                    move || view! { <span class="folder-name">{name.clone()}</span> }
                }
            >
                {
                    finished.set_value(false);
                    let start = name.clone();
                    view! {
                        <input
                            class="input sm folder-rename"
                            node_ref=rename_box
                            value=start
                            aria-label="Folder name"
                            on:click=|ev| ev.stop_propagation()
                            on:keydown=move |ev| {
                                ev.stop_propagation();
                                match ev.key().as_str() {
                                    "Enter" => {
                                        finished.set_value(true);
                                        env.rename(id, event_target_value(&ev));
                                    }
                                    "Escape" => {
                                        finished.set_value(true);
                                        env.renaming.set(None);
                                    }
                                    _ => {}
                                }
                            }
                            on:blur=move |ev| {
                                if !finished.get_value() {
                                    finished.set_value(true);
                                    env.rename(id, event_target_value(&ev));
                                }
                            }
                        />
                    }
                }
            </Show>
            <span class="count">{row.shown}</span>
            <span class="thread-row-menu" on:click=|ev| ev.stop_propagation()>
                <RowMenu items=menu title="Folder actions"/>
            </span>
        </div>
    }
}

/// The "No folder" drop zone: takes a dragged thread out of its folder. Only
/// there while something is being dragged, and stuck to the list's bottom
/// edge so its appearing moves nothing above it.
#[component]
pub(super) fn NoFolderZone(env: FolderEnv) -> impl IntoView {
    let over = RwSignal::new(false);
    view! {
        <Show when=move || env.dragged.get().is_some()>
            <div
                class="thread-dropzone"
                class:drop-over=move || over.get()
                on:dragover=move |ev| {
                    ev.prevent_default();
                    over.set(true);
                }
                on:dragleave=move |_| over.set(false)
                on:drop=move |ev| {
                    ev.prevent_default();
                    over.set(false);
                    if let Some(t) = env.dragged.get_untracked() {
                        env.dragged.set(None);
                        env.move_thread(t, None);
                    }
                }
            >
                "No folder — drop here to take it out of its folder"
            </div>
        </Show>
    }
}

/// The three dialogs, mounted once by the page.
#[component]
pub(super) fn FolderDialogs(env: FolderEnv) -> impl IntoView {
    view! {
        <Modal open=env.settings_open title="Folder settings" guard=true>
            <Show when=move || env.settings_open.get()>
                {move || {
                    env.settings_of
                        .get_untracked()
                        .and_then(|id| env.folder(id))
                        .map(|f| view! { <FolderSettingsForm env=env folder=f/> })
                }}
            </Show>
        </Modal>
        <Modal open=env.delete_open title="Delete folder">
            <Show when=move || env.delete_open.get()>
                {move || {
                    env.delete_of
                        .get_untracked()
                        .and_then(|id| env.folder(id))
                        .map(|f| view! { <FolderDeleteForm env=env folder=f/> })
                }}
            </Show>
        </Modal>
        <Modal open=env.move_open title="Move to folder">
            <Show when=move || env.move_open.get()>
                {move || {
                    env.move_of.get_untracked().map(|t| view! { <MovePicker env=env thread=t/> })
                }}
            </Show>
        </Modal>
    }
}

/// Name + the thread settings' fields. What is set here is what a new chat
/// in the folder starts with; chats already in it are not touched.
#[component]
fn FolderSettingsForm(env: FolderEnv, folder: FolderInfo) -> impl IntoView {
    let id = folder.id;
    let name = RwSignal::new(folder.name.clone());
    let seeded = thread_from_defaults(&folder.defaults);
    let model = RwSignal::new(seeded.model_alias.clone());
    let draft = SettingsDraft::new();
    draft.seed(&seeded);
    let errors = DraftErrors::of(draft);
    let busy = RwSignal::new(false);
    let toasts = env.toasts;
    let save = move |_| {
        let patch = match draft_patch(&draft) {
            Ok(p) => p,
            Err(e) => return toasts.err(e),
        };
        let n = name.get_untracked().trim().to_string();
        if n.is_empty() {
            return toasts.err("a folder needs a name");
        }
        let body = json!({
            "name": n,
            "defaults": defaults_body(patch, &model.get_untracked()),
        });
        busy.set(true);
        env.scope.spawn(async move {
            match crate::api::post::<Value, _>(format!("/chat/api/folders/{id}"), &body).await {
                Ok(_) => {
                    env.settings_open.set(false);
                    toasts.ok("folder settings saved");
                    env.refresh.run(());
                }
                Err(e) => toasts.err(format!("saving the folder failed: {e}")),
            }
            busy.try_set(false);
        });
    };
    view! {
        <div class="chat-settings density-dense folder-settings">
            <div class="field">
                <label>"Name"</label>
                <input
                    class="input"
                    prop:value=move || name.get()
                    on:input=move |ev| name.set(event_target_value(&ev))
                />
            </div>
            <p class="dim">
                "New chats in this folder start with these settings. Chats already in it keep theirs."
            </p>
            <div class="field">
                <label>"Model"</label>
                <ModelPicker
                    value=model
                    tasks=&["chat"]
                    recent_key="chat"
                    empty_label="the model picked when starting a chat".to_string()
                />
            </div>
            <SettingsFields
                draft=draft
                errors=errors
                model=model
                prompt_label="System prompt — blank keeps the default prompt"
            />
        </div>
        <ModalFooter>
            <button class="btn ghost" on:click=move |_| env.settings_open.set(false)>
                "Cancel"
            </button>
            <button
                class="btn primary"
                disabled=move || errors.any() || busy.try_get().unwrap_or(false)
                on:click=save
            >
                "Save"
            </button>
        </ModalFooter>
    }
}

/// Keep the chats (they lose their folder) or delete them with it.
#[component]
fn FolderDeleteForm(env: FolderEnv, folder: FolderInfo) -> impl IntoView {
    let id = folder.id;
    let total = folder.threads_active + folder.threads_archived;
    let what = if total == 0 {
        "It holds no chats.".to_string()
    } else if folder.threads_archived > 0 {
        format!(
            "It holds {total} chat{} ({} archived).",
            if total == 1 { "" } else { "s" },
            folder.threads_archived
        )
    } else {
        format!(
            "It holds {total} chat{}.",
            if total == 1 { "" } else { "s" }
        )
    };
    view! {
        <p>
            <strong>{folder.name.clone()}</strong>
            " — "
            {what}
        </p>
        <ModalFooter>
            <button class="btn ghost" on:click=move |_| env.delete_open.set(false)>
                "Cancel"
            </button>
            {if total == 0 {
                view! {
                    <ConfirmButton
                        label="Delete folder"
                        confirm="Delete the folder?"
                        class="btn danger"
                        on_confirm=Callback::new(move |()| env.delete(id, false))
                    />
                }
                    .into_any()
            } else {
                view! {
                    <button
                        class="btn"
                        title="The chats stay, without a folder"
                        on:click=move |_| env.delete(id, false)
                    >
                        "Delete folder, keep the chats"
                    </button>
                    <ConfirmButton
                        label=format!("Delete folder and its {total} chats")
                        confirm="Delete the chats too?"
                        class="btn danger"
                        on_confirm=Callback::new(move |()| env.delete(id, true))
                    />
                }
                    .into_any()
            }}
        </ModalFooter>
    }
}

/// "Move to…": No folder, or any folder.
#[component]
fn MovePicker(env: FolderEnv, thread: i64) -> impl IntoView {
    let here = env.current.with_untracked(|c| {
        c.as_ref()
            .filter(|t| t.id == thread)
            .and_then(|t| t.folder_id)
    });
    let choose = move |folder: Option<i64>| {
        env.move_open.set(false);
        env.move_thread(thread, folder);
    };
    let folders = env.folders.get_untracked();
    view! {
        <div class="move-list">
            <button class="btn ghost" on:click=move |_| choose(None)>
                "No folder"
            </button>
            {folders
                .into_iter()
                .map(|f| {
                    let id = f.id;
                    view! {
                        <button
                            class="btn ghost"
                            disabled=here == Some(id)
                            on:click=move |_| choose(Some(id))
                        >
                            {f.name}
                        </button>
                    }
                })
                .collect_view()}
            {env
                .folders
                .with_untracked(Vec::is_empty)
                .then(|| view! { <p class="dim">"No folders yet — make one with New folder."</p> })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::super::chat::group_threads;
    use super::*;

    /// `thread_items` with the clock left out: a row is the thread alone, and
    /// a date heading is the day it was last active.
    fn thread_items(
        threads: &[ChatThread],
        folders: &[FolderInfo],
        collapsed: &HashSet<i64>,
        query: &str,
        _today: &str,
        archived: bool,
    ) -> Vec<ListItem> {
        group_threads(
            threads,
            folders,
            collapsed,
            query,
            archived,
            &|t| ListItem::Row {
                thread: t.clone(),
                when: String::new(),
                when_title: String::new(),
                nested: false,
                folder: None,
            },
            &|t| t.updated_at[..10].to_string(),
        )
    }

    fn folder(id: i64, name: &str) -> FolderInfo {
        FolderInfo {
            id,
            name: name.into(),
            ..Default::default()
        }
    }

    fn thread(id: i64, title: &str, folder: Option<i64>, pinned: bool, at: &str) -> ChatThread {
        ChatThread {
            id,
            title: title.into(),
            model_alias: "m".into(),
            folder_id: folder,
            pinned,
            updated_at: format!("2026-09-{at} 10:00:00"),
            ..Default::default()
        }
    }

    fn ids(items: &[ListItem]) -> Vec<i64> {
        items
            .iter()
            .filter_map(|i| match i {
                ListItem::Row { thread, .. } => Some(thread.id),
                _ => None,
            })
            .collect()
    }

    fn heads(items: &[ListItem]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                ListItem::Head(l, _) => Some(l.clone()),
                ListItem::Folder(f) => Some(format!("folder:{}", f.info.name)),
                _ => None,
            })
            .collect()
    }

    fn sample() -> Vec<ChatThread> {
        vec![
            thread(1, "loose pinned", None, true, "29"),
            thread(2, "in work, old", Some(10), false, "20"),
            thread(3, "in work, pinned", Some(10), true, "10"),
            thread(4, "in work, new", Some(10), false, "28"),
            thread(5, "loose", None, false, "27"),
            thread(6, "in home", Some(11), false, "26"),
        ]
    }

    #[test]
    fn a_thread_appears_exactly_once() {
        let folders = [folder(10, "Work"), folder(11, "Home"), folder(12, "Empty")];
        let items = thread_items(
            &sample(),
            &folders,
            &HashSet::new(),
            "",
            "2026-09-30",
            false,
        );
        let mut listed = ids(&items);
        listed.sort_unstable();
        assert_eq!(listed, [1, 2, 3, 4, 5, 6]);
        // Folders lead (empty ones too), then Pinned, then dates.
        assert_eq!(
            heads(&items)[..4],
            ["folder:Work", "folder:Home", "folder:Empty", "Pinned"]
        );
    }

    #[test]
    fn pinned_in_a_folder_stays_in_its_folder_on_top() {
        let folders = [folder(10, "Work")];
        let items = thread_items(
            &sample(),
            &folders,
            &HashSet::new(),
            "",
            "2026-09-30",
            false,
        );
        assert_eq!(ids(&items)[..3], [3, 4, 2], "pinned, then newest first");
        // The Pinned group holds only the unfiled pinned thread.
        let pinned_at = items
            .iter()
            .position(|i| matches!(i, ListItem::Head(l, _) if l == "Pinned"))
            .unwrap();
        assert!(matches!(&items[pinned_at + 1], ListItem::Row { thread, .. } if thread.id == 1));
        assert!(matches!(&items[pinned_at + 2], ListItem::Head(l, _) if l != "Pinned"));
        // Its pin mark survives in the folder.
        assert!(matches!(&items[1], ListItem::Row { thread, nested: true, .. } if thread.pinned));
    }

    #[test]
    fn a_collapsed_folder_hides_its_threads_but_keeps_its_count() {
        let folders = [folder(10, "Work")];
        let collapsed: HashSet<i64> = [10].into();
        let items = thread_items(&sample(), &folders, &collapsed, "", "2026-09-30", false);
        assert_eq!(ids(&items), [1, 5, 6]);
        assert!(matches!(&items[0], ListItem::Folder(f) if f.collapsed && f.shown == 3));
    }

    #[test]
    fn the_filter_matches_folder_names_and_opens_folders() {
        let folders = [folder(10, "Work"), folder(11, "Home")];
        let collapsed: HashSet<i64> = [10, 11].into();
        // A folder's name matches every thread in it, collapsed or not.
        let items = thread_items(&sample(), &folders, &collapsed, "work", "2026-09-30", false);
        assert_eq!(heads(&items)[0], "folder:Work");
        assert_eq!(ids(&items), [3, 4, 2]);
        assert!(!heads(&items).contains(&"folder:Home".to_string()));
        // A title inside one folder shows just that thread, inside it.
        let items = thread_items(
            &sample(),
            &folders,
            &collapsed,
            "in home",
            "2026-09-30",
            false,
        );
        assert_eq!(ids(&items), [6]);
        assert!(matches!(&items[0], ListItem::Folder(f) if f.info.name == "Home" && f.shown == 1));
        // An empty folder is found by its name.
        let empty = [folder(12, "Empty")];
        let items = thread_items(&[], &empty, &HashSet::new(), "emp", "2026-09-30", false);
        assert_eq!(heads(&items), ["folder:Empty"]);
    }

    #[test]
    fn the_archived_view_is_flat_and_names_the_folder() {
        let folders = [folder(10, "Work")];
        let mut ts = sample();
        for t in &mut ts {
            t.archived_at = Some("2026-09-29 09:00:00".into());
        }
        let items = thread_items(&ts, &folders, &HashSet::new(), "", "2026-09-30", true);
        assert_eq!(items.len(), 6, "no headings");
        let named: Vec<Option<String>> = items
            .iter()
            .map(|i| match i {
                ListItem::Row { folder, .. } => folder.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(named[1].as_deref(), Some("Work"));
        assert_eq!(named[0], None);
    }

    #[test]
    fn a_thread_in_an_unknown_folder_is_listed_like_an_unfiled_one() {
        let items = thread_items(
            &[thread(1, "orphan", Some(99), false, "27")],
            &[],
            &HashSet::new(),
            "",
            "2026-09-30",
            false,
        );
        assert_eq!(ids(&items), [1]);
        assert!(matches!(&items[0], ListItem::Head(_, 1)));
    }

    #[test]
    fn defaults_seed_a_thread_and_a_blank_form_sends_nulls() {
        let t = thread_from_defaults(&json!({
            "model_alias": "m", "top_k": 40, "temperature": null, "stop": null,
            "mcp_tools": [{"server_label": "s", "allowed_tools": null}]
        }));
        assert_eq!(
            (t.model_alias.as_str(), t.top_k, t.temperature),
            ("m", Some(40), None)
        );
        assert_eq!(t.mcp_tools.len(), 1);
        let body = defaults_body(
            json!({"system_prompt": "", "mcp_tools": [], "top_k": 5}),
            "",
        );
        assert_eq!(body["system_prompt"], Value::Null);
        assert_eq!(body["mcp_tools"], Value::Null);
        assert_eq!(body["model_alias"], Value::Null);
        assert_eq!(body["top_k"], 5);
    }
}
