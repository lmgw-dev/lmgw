//! The Examples menu (labs-store design §2.8): the built-in forms — ticket
//! triage, sentiment, "is the user done talking?" — and an empty form. A
//! pick replaces the form; while the form has edits the pick asks first
//! (the menu's own second pick).

use leptos::prelude::*;
use lmgw_api_types::decision_lab::{examples, DecisionForm};

use super::page::Lab;
use crate::widgets::{MenuItem, RowMenu};

/// Load `form`, dropping the override (it was the old form's request).
fn load(lab: Lab, form: DecisionForm) {
    lab.form.load(form, &[]);
    lab.override_text.set(None);
}

#[component]
pub(super) fn ExamplesMenu(lab: Lab) -> impl IntoView {
    let items = Signal::derive(move || {
        let dirty = lab.form.dirty();
        let item = |label: &str, form: DecisionForm| {
            let it = MenuItem::new(label.to_string(), move || load(lab, form.clone()));
            if dirty {
                it.danger()
                    .title("Replaces the form, which has edits: pick again to replace it")
            } else {
                it.title("Replaces the form")
            }
        };
        examples()
            .into_iter()
            .map(|e| item(e.name, e.form))
            .chain(std::iter::once(item("Empty form", DecisionForm::default())))
            .collect::<Vec<_>>()
    });
    view! {
        <div class="dl-toolbar">
            <RowMenu
                items=items
                title="Load a built-in example into the form"
                label=Signal::derive(|| "Examples ▾".to_string())
            />
            <span class="mini-note dim">
                {move || if lab.form.dirty() { "edited" } else { "" }}
            </span>
        </div>
    }
}
