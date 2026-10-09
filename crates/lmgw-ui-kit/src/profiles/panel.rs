//! [`ProfilesPanel`]: the profiles list with the editor beside it.

mod switch;

use leptos::prelude::*;
use lmgw_api_types::chat_profiles::{Profile, ProfileList, BUILTIN_CONCISE};

use self::switch::{after_stored, asks, back_from_new, upsert, Pick};
use super::api;
use super::editor::ProfileEditor;
use crate::scope::Scope;
use crate::widgets::{Modal, ModalFooter};

/// The list of profiles (name order, as the gateway answers it) with the
/// editor for the picked one, "New profile", and a way to add the built-in
/// back after it was deleted. It reads and writes only the profile routes.
///
/// `thread_id` and `test_model` go to every editor ([`ProfileEditor`]);
/// `on_changed` gets the fresh list after any create, save or delete, so
/// a host's own pickers can follow. `dirty` is kept `true` while the editor
/// has unsaved edits, for a host that guards leaving its page; switching to
/// another profile (or "New profile") with unsaved edits asks first here.
/// Discard on a new profile drops the draft and goes back to what the pane
/// showed before "New profile".
#[component]
pub fn ProfilesPanel(
    #[prop(default = None)] thread_id: Option<i64>,
    #[prop(default = None, into)] test_model: Option<String>,
    #[prop(optional)] on_changed: Option<Callback<ProfileList>>,
    #[prop(optional)] dirty: Option<RwSignal<bool>>,
) -> impl IntoView {
    let list = RwSignal::new(ProfileList::default());
    let loaded = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let pick = RwSignal::new(Pick::Nothing);
    let dirty = dirty.unwrap_or_else(|| RwSignal::new(false));
    // A switch held for the question (unsaved edits), and the question.
    let held = RwSignal::new(None::<Pick>);
    let asking = RwSignal::new(false);
    // What the pane showed before "New profile": a draft's Discard goes back to it.
    let before_new = StoredValue::new(Pick::Nothing);
    let scope = Scope::new();

    let refresh = move || {
        scope.spawn(async move {
            match api::list().await {
                Ok(l) => {
                    list.set(l.clone());
                    loaded.set(true);
                    error.set(None);
                    if let Some(cb) = on_changed {
                        cb.run(l);
                    }
                }
                Err(e) => {
                    loaded.set(true);
                    error.set(Some(e.to_string()));
                }
            }
        });
    };
    refresh();

    // The pane on another pick: its editor's edits are gone with it.
    let switch = move |to: Pick| {
        let from = pick.get_untracked();
        if from != to {
            if to == Pick::New {
                before_new.set_value(from);
            }
            dirty.set(false);
            pick.set(to);
        }
    };
    let ask_or_switch = move |to: Pick| {
        if asks(pick.get_untracked(), to, dirty.get_untracked()) {
            held.set(Some(to));
            asking.set(true);
        } else {
            switch(to);
        }
    };
    // Closing the question any way but "Discard" stays.
    Effect::new(move |_| {
        if !asking.get() {
            held.set(None);
        }
    });
    let discard_and_switch = move |_| {
        if let Some(to) = held.get_untracked() {
            switch(to);
        }
        asking.set(false);
    };
    // A stored row (a create, a save, a reset): into the list first, so an
    // editor that mounts on it shows what was stored; the editor that wrote
    // it already shows it and is not mounted again.
    let stored = move |p: Profile| {
        let id = p.id;
        list.update(|l| upsert(l, p));
        if let Some(to) = after_stored(pick.get_untracked(), id) {
            switch(to);
        }
        refresh();
    };

    let has_builtin = move || {
        list.with(|l| {
            l.profiles
                .iter()
                .any(|p| p.builtin.as_deref() == Some(BUILTIN_CONCISE))
        })
    };
    let add_builtin = move |_| {
        scope.spawn(async move {
            match api::create_builtin(BUILTIN_CONCISE).await {
                Ok(p) => {
                    let id = p.id;
                    list.update(|l| upsert(l, p));
                    ask_or_switch(Pick::Id(id));
                    refresh();
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
    let saved = Callback::new(stored);
    let discard_new = Callback::new(move |_| {
        switch(list.with_untracked(|l| back_from_new(before_new.get_value(), l)));
    });
    let deleted = Callback::new(move |_| {
        switch(Pick::Nothing);
        refresh();
    });

    view! {
        <div class="pf-panel">
            <div class="pf-list">
                <div class="pf-list-head">
                    <button type="button" class="btn sm" on:click=move |_| ask_or_switch(Pick::New)>
                        "New profile"
                    </button>
                    <Show when=move || loaded.get() && !has_builtin()>
                        <button type="button" class="btn ghost sm" on:click=add_builtin>
                            "Add built-in Concise"
                        </button>
                    </Show>
                </div>
                {move || error.get().map(|e| view! { <div class="field-err" role="alert">{e}</div> })}
                <Show when=move || loaded.get() && list.with(|l| l.profiles.is_empty())>
                    <div class="field-hint">"No profiles yet. Threads use none: the default behaviour."</div>
                </Show>
                <For each=move || list.get().profiles key=|p| (p.id, p.name.clone()) let:p>
                    {
                        let id = p.id;
                        let builtin = p.builtin.is_some();
                        let name = p.name.clone();
                        view! {
                            <button
                                type="button"
                                class="pf-item"
                                class:active=move || pick.get() == Pick::Id(id)
                                on:click=move |_| ask_or_switch(Pick::Id(id))
                            >
                                <span class="pf-item-name">{name}</span>
                                {builtin.then(|| view! { <span class="chip info">"built-in"</span> })}
                                {move || {
                                    (list.with(|l| l.default_profile_id) == Some(id))
                                        .then(|| view! { <span class="chip off">"default"</span> })
                                }}
                            </button>
                        }
                    }
                </For>
            </div>
            <div class="pf-pane">
                {move || {
                    let p = pick.get();
                    // Only the pick remounts the editor; a refreshed list does not.
                    // A stored row is in the list before the pick moves to it.
                    let found = match p {
                        Pick::Id(id) => list.with_untracked(|l| l.profiles.iter().find(|x| x.id == id).cloned()),
                        _ => None,
                    };
                    let is_default = match p {
                        Pick::Id(id) => list.with_untracked(|l| l.default_profile_id) == Some(id),
                        _ => false,
                    };
                    match (p, found) {
                        (Pick::New, _) => {
                            Some(view! {
                                <ProfileEditor
                                    profile=None
                                    thread_id=thread_id
                                    test_model=test_model.clone()
                                    on_saved=saved
                                    on_discard_new=discard_new
                                    dirty=dirty
                                />
                            })
                        }
                        (Pick::Id(_), Some(profile)) => {
                            Some(view! {
                                <ProfileEditor
                                    profile=Some(profile)
                                    thread_id=thread_id
                                    test_model=test_model.clone()
                                    is_default=is_default
                                    on_saved=saved
                                    on_deleted=deleted
                                    dirty=dirty
                                />
                            })
                        }
                        _ => None,
                    }
                }}
            </div>
            <Modal open=asking title="Unsaved changes">
                <p class="dirty-q">
                    "This profile has unsaved changes. Opening another one discards them."
                </p>
                <ModalFooter>
                    <span class="foot-danger">
                        <button type="button" class="btn danger" on:click=discard_and_switch>
                            "Discard and open"
                        </button>
                    </span>
                    <button type="button" class="btn primary" on:click=move |_| asking.set(false)>
                        "Stay"
                    </button>
                </ModalFooter>
            </Modal>
        </div>
    }
}
