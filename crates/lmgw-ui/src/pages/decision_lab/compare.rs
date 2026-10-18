//! Compare and Perturb (labs-store design §2.6): "Also ask on" more models,
//! and re-runs with the option descriptions dropped and with the option
//! order reversed. The plan is the models × (as written, then each checked
//! variant), models outer. Perturb is off, with the reason, while an
//! override is in effect: rewriting an override would mean parsing it into
//! a typed request, which drops unknown fields.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::PERTURB_WITH_OVERRIDE;

use super::head::entries;
use super::page::Lab;
use crate::widgets::ModelPicker;

#[component]
pub(super) fn CompareCard(lab: Lab) -> impl IntoView {
    let add = RwSignal::new(String::new());
    // A pick joins the list and the picker empties again.
    Effect::new(move |_| {
        let m = add.get();
        if m.is_empty() {
            return;
        }
        let head = lab.alias.get_untracked();
        lab.compare.update(|v| {
            if m != head && !v.contains(&m) {
                v.push(m.clone());
            }
        });
        add.set(String::new());
    });
    // The head model is never also compared.
    Effect::new(move |_| {
        let head = lab.alias.get();
        if lab.compare.with_untracked(|v| v.contains(&head)) {
            lab.compare.update(|v| v.retain(|m| *m != head));
        }
    });
    let list = Signal::derive(move || {
        let head = lab.alias.get();
        lab.models.with(|v| {
            entries(v)
                .into_iter()
                .filter(|e| e.id != head)
                .collect::<Vec<_>>()
        })
    });
    let status = Signal::derive(move || lab.list.get());
    let off = move || lab.override_text.with(Option::is_some);
    let check = move |sig: RwSignal<bool>, label: &'static str, title: &'static str| {
        view! {
            <label
                class="dl-check"
                class:off=off
                title=move || if off() { PERTURB_WITH_OVERRIDE.to_string() } else { title.to_string() }
            >
                <input
                    type="checkbox"
                    prop:checked=move || sig.get() && !off()
                    disabled=off
                    on:change=move |ev| sig.set(event_target_checked(&ev))
                />
                {label}
            </label>
        }
    };
    view! {
        <div class="card dl-compare">
            <h3 class="lab-h3">"Compare and perturb"</h3>
            <div class="dl-also">
                <span class="lab-label">"Also ask on"</span>
                <For each=move || lab.compare.get() key=|m| m.clone() let:m>
                    {
                        let name = m.clone();
                        view! {
                            <span class="chip dl-model-chip" title=m.clone()>
                                <span class="n">{m.clone()}</span>
                                <button
                                    type="button"
                                    class="chip-x"
                                    title="Do not ask this model"
                                    on:click=move |_| lab.compare.update(|v| v.retain(|x| *x != name))
                                >
                                    "✕"
                                </button>
                            </span>
                        }
                    }
                </For>
                <span class="dl-add-model">
                    <ModelPicker value=add entries=list status=status empty_label="Add model"/>
                </span>
            </div>
            <div class="dl-perturb">
                <span class="lab-label">"Perturb"</span>
                {check(
                    lab.no_descriptions,
                    "descriptions dropped",
                    "Every choice description sent as null, and no noul criteria",
                )}
                {check(lab.reversed, "order reversed", "Each choice question's options in reverse")}
            </div>
            <p class="mini-note dim">
                {move || {
                    if off() {
                        format!("Perturb is off: {PERTURB_WITH_OVERRIDE}.")
                    } else {
                        "Score levels, noul and the question order are never perturbed: reversing a scale inverts it, and the question order is the answer order.".to_string()
                    }
                }}
            </p>
        </div>
    }
}
