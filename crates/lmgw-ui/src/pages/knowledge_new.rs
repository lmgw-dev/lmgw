//! New knowledge base: the dialog. The API validates before the row exists —
//! the model must resolve and answer a probe (that is how its identity is
//! pinned), and a chunk size above what it takes per input is refused — so a
//! failure here is the server's own sentence, printed as it came.

use leptos::prelude::*;

use crate::widgets::{use_modal, use_toasts, Modal, ModalFooter};

use super::knowledge::Kb;
use super::knowledge_form::{KbFields, KbForm};

#[component]
pub fn NewKbModal(open: RwSignal<bool>, on_created: Callback<()>) -> impl IntoView {
    view! {
        <Modal open=open title="New knowledge base" guard=true>
            // Rebuilt on every open, so a second base does not start from the
            // first one's typing.
            {move || open.get().then(|| view! { <NewForm open=open on_created=on_created/> })}
        </Modal>
    }
}

#[component]
fn NewForm(open: RwSignal<bool>, on_created: Callback<()>) -> impl IntoView {
    let toasts = use_toasts();
    let navigate = leptos_router::hooks::use_navigate();
    let scope = crate::scope::Scope::new();
    let form = KbForm::blank();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let modal = use_modal();

    let create = Callback::new(move |()| {
        if busy.get_untracked() {
            return;
        }
        let body = match form.body() {
            Ok(b) => b,
            Err(e) => {
                error.set(Some(e));
                return;
            }
        };
        busy.set(true);
        error.set(None);
        let navigate = navigate.clone();
        leptos::task::spawn_local(async move {
            let res = crate::api::post::<Kb, _>("/api/knowledge/bases", &body).await;
            match res {
                Ok(kb) => {
                    toasts.ok(format!("knowledge base '{}' created", kb.name));
                    if scope.alive() {
                        busy.set(false);
                        if let Some(m) = modal {
                            m.saved();
                        }
                        open.set(false);
                        on_created.run(());
                    }
                    navigate(&format!("/knowledge/{}", kb.id), Default::default());
                }
                Err(e) => {
                    if scope.alive() {
                        busy.set(false);
                        error.set(Some(e.to_string()));
                    }
                }
            }
        });
    });

    view! {
        <KbFields form=form/>
        {move || error.get().map(|e| view! { <div class="wiz-err">{e}</div> })}
        <ModalFooter>
            <button class="btn ghost" on:click=move |_| open.set(false)>
                "Cancel"
            </button>
            <button class="btn primary" disabled=move || busy.get() on:click=move |_| create.run(())>
                {move || if busy.get() { "Creating…" } else { "Create knowledge base" }}
            </button>
        </ModalFooter>
    }
}
