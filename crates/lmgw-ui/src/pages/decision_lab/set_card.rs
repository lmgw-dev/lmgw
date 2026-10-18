//! The form column's question-set card (labs-store design §3.2): which set
//! the questions came from and whether they were edited since, "Load ▾",
//! "Save as set…" (a name), "Update set" (the loaded set rewritten, or the
//! 409 `set_changed` said with Reload), and "New set run…", which runs the
//! questions over a file. Also each question card's threshold line.

use leptos::html;
use leptos::prelude::*;
use leptos_router::components::A;
use lmgw_api_types::decision_lab::check_questions;

use super::batch_new::open_new_run;
use super::page::Lab;
use super::sets::{self, edits, set_of, threshold_text, Edits};
use crate::widgets::{MenuItem, RowMenu};

/// How the form differs from the loaded set (tracked); `None` without one.
pub(super) fn loaded_edits(lab: Lab) -> Option<Edits> {
    let questions = lab.form.question_drafts();
    let model = lab.alias.get();
    lab.sets
        .loaded
        .with(|l| l.as_ref().map(|p| edits(set_of(p), &questions, &model)))
}

/// Why "New set run…" cannot open yet; `None` when it can.
pub(super) fn run_blocked(lab: Lab) -> Option<String> {
    if lab.alias.with(|a| a.trim().is_empty()) {
        return Some("pick a model".into());
    }
    let qs = lab.form.question_drafts();
    check_questions(&qs).err()
}

#[component]
pub(super) fn SetCard(lab: Lab) -> impl IntoView {
    let naming = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let err = RwSignal::new(None::<String>);
    let input: NodeRef<html::Input> = NodeRef::new();
    let edit = Memo::new(move |_| loaded_edits(lab));
    let blocked = Memo::new(move |_| run_blocked(lab));
    // The questions differ from what was loaded: a load asks first.
    let questions_dirty = Memo::new(move |_| sets::questions_dirty(lab));
    let load_items = Signal::derive(move || {
        let dirty = questions_dirty.get();
        lab.sets
            .list
            .get()
            .into_iter()
            .map(|p| {
                let n = set_of(&p).questions.len();
                let label = format!(
                    "{} · {n} {}",
                    p.name,
                    if n == 1 { "question" } else { "questions" }
                );
                let it = MenuItem::new(label, move || sets::load_set(lab, p.clone()));
                if dirty {
                    it.danger().title(
                        "Replaces the questions, which have edits: pick again to replace them",
                    )
                } else {
                    it.title("Replaces the questions and picks the set's model")
                }
            })
            .collect::<Vec<_>>()
    });
    let save = move || {
        let n = name.get_untracked();
        if n.trim().is_empty() {
            err.set(Some("give the set a name".into()));
            return;
        }
        sets::save_as(
            lab,
            n,
            err,
            Callback::new(move |()| {
                naming.set(false);
                name.set(String::new());
            }),
        );
    };
    Effect::new(move |_| {
        if naming.get() {
            if let Some(i) = input.get() {
                let _ = i.focus();
            }
        }
    });

    view! {
        <div class="card dl-setcard">
            <div class="dl-card-head">
                <h3 class="lab-h3">"Question set"</h3>
                <span class="dl-setname" title="The set the questions came from">
                    {move || {
                        lab.sets
                            .loaded
                            .with(|l| l.as_ref().map(|p| p.name.clone()))
                            .unwrap_or_else(|| "none loaded".into())
                    }}
                </span>
                <span class="mini-note dim dl-setedit">
                    {move || edit.get().map(Edits::text).unwrap_or("")}
                </span>
                <span class="spacer"></span>
                <Show when=move || lab.sets.list.with(|v| !v.is_empty())>
                    <RowMenu
                        items=load_items
                        title="Load a saved set: its questions replace the form's, its model becomes the head model"
                        label=Signal::derive(|| "Load ▾".to_string())
                    />
                </Show>
            </div>
            <div class="dl-setbtns">
                <button
                    type="button"
                    class="btn ghost sm"
                    title="Keep the form's questions and the head model under a name"
                    on:click=move |_| {
                        err.set(None);
                        naming.update(|v| *v = !*v);
                    }
                >
                    "Save as set…"
                </button>
                <button
                    type="button"
                    class="btn ghost sm"
                    disabled=move || !edit.get().is_some_and(Edits::any)
                    title=move || match lab.sets.loaded.get() {
                        None => "Load or save a set first".to_string(),
                        Some(p) => format!(
                            "Overwrite the set '{}' with the form's questions and the head model; a changed question keeps its threshold's value, not its measurement",
                            p.name
                        ),
                    }
                    on:click=move |_| sets::update_loaded(lab)
                >
                    "Update set"
                </button>
                <span class="spacer"></span>
                <button
                    type="button"
                    class="btn sm"
                    disabled=move || blocked.get().is_some()
                    title=move || match blocked.get() {
                        Some(why) => format!("Not yet: {why}"),
                        None => "Ask these questions of every row of a CSV or JSONL file, as a job".into(),
                    }
                    on:click=move |_| open_new_run(lab)
                >
                    "New set run…"
                </button>
            </div>
            <Show when=move || naming.get()>
                <div class="dl-setname-row">
                    <input
                        class="input"
                        placeholder="set name"
                        node_ref=input
                        prop:value=move || name.get()
                        on:input=move |ev| name.set(event_target_value(&ev))
                        on:keydown=move |ev: web_sys::KeyboardEvent| {
                            match ev.key().as_str() {
                                "Enter" => save(),
                                "Escape" => naming.set(false),
                                _ => {}
                            }
                        }
                    />
                    <button type="button" class="btn primary sm" on:click=move |_| save()>
                        "Save"
                    </button>
                    <button type="button" class="btn ghost sm" on:click=move |_| naming.set(false)>
                        "Cancel"
                    </button>
                </div>
            </Show>
            {move || err.get().map(|e| view! { <p class="mini-note bad">{e}</p> })}
            {move || {
                lab.sets
                    .conflict
                    .get()
                    .map(|msg| {
                        view! {
                            <div class="notice warn dl-set-conflict">
                                <b>"Not updated: the set changed since it was loaded."</b>
                                " "
                                {msg}
                                <div class="row">
                                    <button
                                        type="button"
                                        class="btn sm"
                                        title="Read the set as it is now; its questions replace the form's"
                                        on:click=move |_| sets::reload_loaded(lab)
                                    >
                                        "Reload the set"
                                    </button>
                                    <button
                                        type="button"
                                        class="btn ghost sm"
                                        on:click=move |_| lab.sets.conflict.set(None)
                                    >
                                        "Dismiss"
                                    </button>
                                </div>
                            </div>
                        }
                    })
            }}
        </div>
    }
}

/// A question card's threshold line in the loaded set, with a link to the
/// set run it was measured on.
#[component]
pub(super) fn ThresholdLine(lab: Lab, uid: u32) -> impl IntoView {
    let line = Memo::new(move |_| {
        let q = lab
            .form
            .questions
            .with(|v| v.iter().find(|x| x.uid == uid).map(|q| q.draft()))?;
        lab.sets
            .loaded
            .with(|l| l.as_ref().map(|p| threshold_text(set_of(p), &q)))
    });
    move || {
        line.get().map(|t| {
            view! {
                <span class="mini-note dl-qthr" title="The loaded set's threshold for this question: an answer below it needs attention">
                    {t.text}
                    {t.batch.map(|b| view! {
                        " · "
                        <A href=format!("/decision-lab/batches/{b}")>{format!("open #{b}")}</A>
                    })}
                </span>
            }
        })
    }
}
