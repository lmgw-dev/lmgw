//! "Save as labelled set" on a finished apply run (labs-store design §4,
//! Q4): the rows the apply job received, the reviewer's edits in them,
//! become a Decision lab set run not yet run, which its own page resumes.
//! Offered on an apply run whose rows carry the stamp of the question set
//! that classified them, while the run's job is kept; the server saves them
//! into that set and says why when it cannot (rows of several sets).

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::decision_lab::{BatchFromAgentSaved, BatchFromAgentSkip};
use serde_json::json;

use super::*;

/// Where a saved set run's page is.
pub fn batch_href(id: i64) -> String {
    format!("/decision-lab/batches/{id}")
}

/// What the line beside the link says about a save: the set run, its set,
/// what it holds and the rows left out, each kind with its ids.
pub fn saved_line(s: &BatchFromAgentSaved) -> String {
    let n = s.batch.total;
    let set = match &s.batch.preset_name {
        Some(name) => format!(" of '{name}'"),
        None => String::new(),
    };
    let mut line = format!(
        "Saved as set run #{}{set} · {n} item{} on {}, {} labelled · not run yet",
        s.batch.id,
        if n == 1 { "" } else { "s" },
        s.batch.model,
        s.batch.labelled
    );
    let mut out = Vec::new();
    for (kind, what) in [
        (BatchFromAgentSkip::NeverAsked, "the model never saw"),
        (BatchFromAgentSkip::CallFailed, "whose call failed"),
    ] {
        let ids: Vec<&str> = s
            .skipped
            .iter()
            .filter(|k| k.kind == kind)
            .map(|k| k.id.as_str())
            .collect();
        if !ids.is_empty() {
            out.push(format!(
                "{} row{} {what} ({})",
                ids.len(),
                if ids.len() == 1 { "" } else { "s" },
                ids.join(", ")
            ));
        }
    }
    if !out.is_empty() {
        line.push_str(&format!(" · left out: {}", out.join(", ")));
    }
    if s.set_changed {
        line.push_str(" · the set changed since these rows were classified");
    }
    line
}

/// Whether the rows of a run on screen name the set that classified them,
/// which is what a save from it saves into.
pub fn rows_name_a_set(rows: &[ReviewRow]) -> bool {
    rows.iter().any(|r| r.set.is_some())
}

/// The button and, once saved, the link to the new set run.
#[component]
pub(super) fn SaveLabelled(run: RunState) -> impl IntoView {
    let toasts = crate::widgets::use_toasts();
    let busy = RwSignal::new(false);
    let saved: RwSignal<Option<BatchFromAgentSaved>> = RwSignal::new(None);
    // Offered on a finished apply run whose rows a question set classified.
    let offered = Signal::derive(move || {
        let apply = run.summary.with(|s| {
            s.as_ref()
                .is_some_and(|s| s.phase == "apply" && !is_live(s))
        });
        apply && run.rows.with(|r| rows_name_a_set(r))
    });
    // Another run on screen forgets what this one saved.
    Effect::new(move |prev: Option<Option<i64>>| {
        let job = run.summary.with(|s| s.as_ref().map(|s| s.job_id));
        if prev.is_some_and(|p| p != job) {
            saved.set(None);
        }
        job
    });
    let save = move |_| {
        let Some(job_id) = run.job_id() else {
            return;
        };
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let res = crate::api::post::<BatchFromAgentSaved, _>(
                "/decision-lab/api/batches/from-agent",
                &json!({ "job_id": job_id }),
            )
            .await;
            busy.set(false);
            match res {
                Ok(s) => {
                    toasts.ok(format!("saved as set run #{}", s.batch.id));
                    saved.set(Some(s));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        <Show when=move || offered.get()>
            <button
                class="btn"
                title="Keep the rows this apply received, with your edits, as a labelled set run \
                       in the Decision lab: it measures the set's threshold on your own answers"
                disabled=move || busy.get()
                on:click=save
            >
                {move || if busy.get() { "Saving…" } else { "Save as labelled set" }}
            </button>
        </Show>
        {move || {
            saved
                .get()
                .map(|s| {
                    view! {
                        <span class="action-note">
                            {saved_line(&s)} " · " <a href=batch_href(s.batch.id)>"Open it"</a>
                        </span>
                    }
                })
        }}
    }
}
