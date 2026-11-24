//! New knowledge base: the dialog, the base's fields as preference groups. The API validates before the row exists —
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
    let page = super::knowledge::use_kb_page();
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
                        // Through the page's guard: another base's unsaved
                        // Settings ask before the move leaves them.
                        page.go(&format!("/knowledge/{}", kb.id), false);
                    }
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

    let blocked = move || busy.get() || form.invalid() != 0;
    view! {
        <div class="kb-new">
            <KbFields form=form/>
        </div>
        {move || error.get().map(|e| view! { <div class="wiz-err">{e}</div> })}
        <ModalFooter>
            <button class="btn" on:click=move |_| open.set(false)>
                "Cancel"
            </button>
            <button
                class="btn suggested pill"
                disabled=blocked
                on:click=move |_| create.run(())
            >
                {move || if busy.get() { "Creating…" } else { "Create knowledge base" }}
            </button>
        </ModalFooter>
    }
}
