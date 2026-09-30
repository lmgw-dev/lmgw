//! The owner's notes on a run (`bench_run_set`, dashboard-only): inline on
//! the detail, and in a small modal from the runs table's menu.

use leptos::prelude::*;
use leptos::task::spawn_local;

use super::{use_bn, Bn};
use crate::bench_api as api;
use crate::widgets::{Modal, ModalFooter};

/// Save `notes` on run `id`; `done` runs once it is stored.
fn save(bn: Bn, id: i64, notes: String, saving: RwSignal<bool>, done: Callback<String>) {
    saving.set(true);
    spawn_local(async move {
        let res = api::set_notes(id, notes.clone()).await;
        saving.try_set(false);
        match res {
            Ok(_) => {
                bn.toasts.ok(format!("run {id}: notes saved"));
                if !bn.scope.alive() {
                    return;
                }
                bn.load_runs();
                done.run(notes);
            }
            Err(e) => bn.toasts.err(format!("run {id}: {e}")),
        }
    });
}

/// The notes on the detail: a box and its Save, which lights up once the
/// text differs from what is stored.
#[component]
pub fn NotesEditor(id: i64, stored: String) -> impl IntoView {
    let bn = use_bn();
    let saved = RwSignal::new(stored.clone());
    let text = RwSignal::new(stored);
    let saving = RwSignal::new(false);
    let dirty = move || text.with(|t| saved.with(|s| t.trim() != s.trim()));
    let on_save = move |_| {
        save(
            bn,
            id,
            text.get_untracked().trim().to_string(),
            saving,
            Callback::new(move |n| saved.set(n)),
        )
    };
    view! {
        <div class="card edit-section bn-notes">
            <h3>"Notes"</h3>
            <textarea
                class="input ta"
                placeholder="What this run was for, what changed, what to remember"
                prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
            ></textarea>
            <div class="row bn-notes-foot">
                <span class="spacer"></span>
                <button class="btn ghost sm" disabled=move || !dirty() || saving.get() on:click=move |_| text.set(saved.get_untracked())>
                    "Discard"
                </button>
                <button class="btn primary sm" disabled=move || !dirty() || saving.get() on:click=on_save>
                    {move || if saving.get() { "Saving…" } else { "Save notes" }}
                </button>
            </div>
        </div>
    }
}

/// The runs table's "Notes…".
#[component]
pub fn NotesModal() -> impl IntoView {
    let bn = use_bn();
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        if bn.notes.with(Option::is_some) {
            open.set(true);
        }
    });
    Effect::new(move |prev: Option<bool>| {
        let now = open.get();
        if prev == Some(true) && !now {
            bn.notes.set(None);
        }
        now
    });
    view! {
        <Modal open=open title="Run notes" guard=true>
            {move || bn.notes.get().map(|(id, notes)| view! { <NotesBody id=id notes=notes open=open/> })}
        </Modal>
    }
}

#[component]
fn NotesBody(id: i64, notes: String, open: RwSignal<bool>) -> impl IntoView {
    let text = RwSignal::new(notes);
    let saving = RwSignal::new(false);
    let bn = use_bn();
    let modal = crate::widgets::use_modal();
    view! {
        <p class="dim">{format!("Run {id}. Notes are yours alone: they change nothing about the run or its comparison.")}</p>
        <textarea
            class="input ta bn-notes-ta"
            placeholder="What this run was for, what changed, what to remember"
            prop:value=move || text.get()
            on:input=move |ev| text.set(event_target_value(&ev))
        ></textarea>
        <ModalFooter>
            <button class="btn ghost" on:click=move |_| open.set(false)>"Cancel"</button>
            <button
                class="btn primary"
                disabled=move || saving.get()
                on:click=move |_| {
                    save(
                        bn,
                        id,
                        text.get_untracked().trim().to_string(),
                        saving,
                        Callback::new(move |_| {
                            if let Some(m) = modal {
                                m.saved();
                            }
                            open.set(false);
                        }),
                    )
                }
            >
                {move || if saving.get() { "Saving…" } else { "Save notes" }}
            </button>
        </ModalFooter>
    }
}
