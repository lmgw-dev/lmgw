//! A set run's status and controls (labs-store design §3.3, §3.5): where it
//! is (running, done, canceled, interrupted with its reason, not started),
//! its progress (answered, failed and unreadable of the total), what it ran
//! and how it reads the file, and Cancel, Resume and Delete.
//!
//! Cancel is graceful: the calls in flight finish and are kept. A second
//! Cancel while the job still runs aborts them; the run says when one went
//! out (`cancel_requested`, also after a reload), and the button says the
//! next press aborts. Resume starts a new job for the items without a run,
//! with "retry failed" for those whose run failed; a done run with failed
//! items offers the retry alone.
//!
//! A progress frame (every ≤ 500 ms while the job runs) redraws the bar,
//! the line and the chip alone. The controls are drawn from what they show
//! ([`Controls`]), which a frame does not move, so an armed Delete, a press
//! that straddles a frame and the keyboard focus stay; Delete is outside
//! them all.

use leptos::prelude::*;
use leptos_router::hooks::use_navigate;
use lmgw_api_types::decision_lab::{BatchResume, BatchStatus, BatchView, QuestionKind};
use lmgw_api_types::lab_store::LabUsage;

use super::batch_view::{Progress, RunPage};
use crate::api;
use crate::fmt::{grouped, local_datetime};
use crate::widgets::ConfirmButton;

/// The status chip: its class and words ("interrupted at 340 of 1 000").
pub(super) fn status_chip(status: BatchStatus, p: Progress) -> (&'static str, String) {
    let at = format!("{} of {}", grouped(p.done), grouped(p.total));
    match status {
        BatchStatus::Running => ("chip live", "running".into()),
        BatchStatus::Done => ("chip ok", "done".into()),
        BatchStatus::Canceled => ("chip warn", format!("canceled at {at}")),
        BatchStatus::Interrupted => ("chip warn", format!("interrupted at {at}")),
        BatchStatus::NotStarted => ("chip off", "not started".into()),
    }
}

/// The progress line: "34 of 50 · 31 answered · 2 failed · 1 unreadable ·
/// 16 to go".
pub(super) fn progress_line(p: Progress, running: bool) -> String {
    let mut s = format!(
        "{} of {} · {} answered · {} failed · {} unreadable",
        grouped(p.done),
        grouped(p.total),
        grouped(p.answered()),
        grouped(p.failed),
        grouped(p.unreadable)
    );
    let left = p.total.saturating_sub(p.done);
    if left > 0 {
        s.push_str(&format!(
            " · {} {}",
            grouped(left),
            if running { "to go" } else { "without a run" }
        ));
    }
    s
}

/// What the Cancel button says: once a Cancel went to the running job
/// (`cancel_requested`), the next press aborts the calls in flight.
pub(super) fn cancel_label(requested: bool) -> (&'static str, &'static str) {
    if requested {
        (
            "Abort the calls in flight",
            "The run stops after the calls in flight. Pressing again aborts them: their answers are not kept, and their requests log 499.",
        )
    } else {
        (
            "Cancel",
            "Stop after the calls in flight, which finish and are kept; a second Cancel while it runs aborts them. Resume runs the rest.",
        )
    }
}

/// What the controls show: drawn again only when one of these moves, never
/// on a progress frame alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Controls {
    pub running: bool,
    /// A Cancel went to the running job: the next one aborts.
    pub abort_next: bool,
    /// Canceled, interrupted or not started: Resume.
    pub resumable: bool,
    /// Done with failed items: Retry the N failed.
    pub retry_only: bool,
    /// The failed items the retry names; 0 when no control names them, so
    /// a failure landing while the job runs moves nothing.
    pub failed: u64,
}

impl Controls {
    pub fn of(v: &BatchView, p: Progress) -> Self {
        let running = v.status == BatchStatus::Running;
        let resumable = matches!(
            v.status,
            BatchStatus::Canceled | BatchStatus::Interrupted | BatchStatus::NotStarted
        );
        let retry_only = v.status == BatchStatus::Done && p.failed > 0;
        Self {
            running,
            abort_next: running && (v.cancel_requested || p.cancel_requested),
            resumable,
            retry_only,
            failed: if resumable || retry_only { p.failed } else { 0 },
        }
    }
}

/// Delete's question: "Delete set run #7, its 30 items and their runs?".
pub(super) fn delete_question(id: i64, total: u64, running: bool) -> String {
    format!(
        "Delete set run #{id}, its {} items and their runs{}?",
        grouped(total),
        if running {
            " (the calls in flight are aborted)"
        } else {
            ""
        }
    )
}

/// A question as the facts list names it: "team (choice, 4 options)".
fn question_fact(q: &lmgw_api_types::decision_lab::QuestionDraft) -> String {
    match &q.kind {
        QuestionKind::Choice { options } => format!("{} (choice, {} options)", q.id, options.len()),
        QuestionKind::Score { levels } => format!("{} (score, {} levels)", q.id, levels.len()),
        QuestionKind::Noul { .. } => format!("{} (noul)", q.id),
    }
}

/// The mapping as the facts list names it.
pub(super) fn mapping_fact(v: &BatchView) -> String {
    let m = &v.mapping;
    let mut parts = vec![format!(
        "state '{}' ({})",
        m.state,
        match m.state_mode {
            lmgw_api_types::decision_lab::StateMode::Text => "text",
            lmgw_api_types::decision_lab::StateMode::Json => "JSON",
        }
    )];
    if let Some(k) = &m.key {
        parts.push(format!("key '{k}'"));
    }
    for (q, col) in m.labels.iter() {
        parts.push(format!("{q} ← '{col}'"));
    }
    parts.join(" · ")
}

fn when(created_at: &str) -> String {
    let ms = js_sys::Date::parse(created_at);
    if ms.is_nan() {
        created_at.to_string()
    } else {
        local_datetime(ms / 1000.0)
    }
}

/// The chip in the page's head.
#[component]
pub(super) fn RunHead(page: RunPage) -> impl IntoView {
    move || {
        let status = page.view.with(|v| v.as_ref().map(|v| v.status))?;
        let (class, text) = status_chip(status, page.progress().unwrap_or_default());
        Some(view! {
            <span class=format!("{class} dl-run-status")>
                {(status == BatchStatus::Running).then(|| view! { <i class="dot"></i> })}
                {text}
            </span>
        })
    }
}

/// The segmented progress bar: answered, unreadable, failed, the rest on
/// the track. Only a part that holds a run is drawn, so the 2 px gap
/// between touching fills is never a gap around nothing.
#[component]
fn ProgressBar(p: Progress) -> impl IntoView {
    let total = p.total.max(1) as f64;
    let part = |n: u64, class: &'static str, what: &'static str| {
        (n > 0).then(|| {
            view! {
                <i
                    class=class
                    style=format!("width:{:.3}%", n as f64 / total * 100.0)
                    title=format!("{} {what}", grouped(n))
                ></i>
            }
        })
    };
    view! {
        <div class="dl-prog" role="img" aria-label=progress_line(p, false)>
            {part(p.answered(), "ok", "answered")}
            {part(p.unreadable, "unread", "unreadable")}
            {part(p.failed, "fail", "failed")}
        </div>
    }
}

#[component]
pub(super) fn StatusCard(page: RunPage) -> impl IntoView {
    let concurrency = RwSignal::new(String::new());
    let retry = RwSignal::new(false);
    // The field starts at the run's own concurrency.
    Effect::new(move |_| {
        if let Some(c) = page.view.with(|v| v.as_ref().map(|v| v.concurrency)) {
            if concurrency.get_untracked().is_empty() {
                concurrency.set(c.to_string());
            }
        }
    });
    let navigate = StoredValue::new(use_navigate());
    let id = page.id;
    // Each part tracks only what it shows: the progress moves on every
    // frame, the controls and the facts on a read of the run.
    let progress = Memo::new(move |_| page.progress().unwrap_or_default());
    let controls = Memo::new(move |_| {
        page.view
            .with(|v| v.as_ref().map(|v| Controls::of(v, progress.get())))
            .unwrap_or_default()
    });
    let total = Memo::new(move |_| page.view.with(|v| v.as_ref().map_or(0, |v| v.total)));

    let cancel = move |_| {
        let abort = progress.get_untracked().cancel_requested;
        page.busy.set(true);
        let ticket = page.control();
        page.scope.spawn(async move {
            let got =
                api::post::<BatchView, _>(format!("/decision-lab/api/batches/{id}/cancel"), &())
                    .await;
            page.busy.set(false);
            match got {
                Ok(v) => {
                    page.take(ticket, v);
                    page.toasts.ok(if abort {
                        "aborting the calls in flight"
                    } else {
                        "canceling: the calls in flight finish first"
                    });
                }
                Err(e) => {
                    page.toasts.err(format!("Cancel failed: {e}"));
                    page.refresh();
                }
            }
        });
    };
    let resume = move |retry_failed: bool| {
        let n = concurrency.get_untracked().trim().parse::<u32>().ok();
        let body = BatchResume {
            concurrency: n,
            retry_failed,
        };
        page.busy.set(true);
        let ticket = page.control();
        page.scope.spawn(async move {
            let got =
                api::post::<BatchView, _>(format!("/decision-lab/api/batches/{id}/resume"), &body)
                    .await;
            page.busy.set(false);
            match got {
                Ok(v) => {
                    page.take(ticket, v);
                    page.toasts.ok("resumed");
                }
                Err(e) => {
                    page.toasts.err(format!("Resume failed: {e}"));
                    page.refresh();
                }
            }
        });
    };
    let delete = Callback::new(move |()| {
        page.busy.set(true);
        page.scope.spawn(async move {
            let got =
                api::post::<LabUsage, _>(format!("/decision-lab/api/batches/{id}/delete"), &())
                    .await;
            page.busy.set(false);
            match got {
                Ok(_) => {
                    page.toasts.ok(format!("set run #{id} deleted"));
                    navigate.with_value(|n| n("/decision-lab", Default::default()));
                }
                Err(e) => page.toasts.err(format!("Delete failed: {e}")),
            }
        });
    });
    let conc_ok = move || {
        concurrency
            .with(|c| c.trim().parse::<u32>().ok())
            .is_some_and(|n| n > 0)
    };
    let delete_confirm = move || delete_question(id, total.get(), controls.get().running);

    // The buttons and fields of `c`: drawn again only when `c` moves.
    let buttons = move || {
        let c = controls.get();
        let (cancel_text, cancel_title) = cancel_label(c.abort_next);
        let failed_n = c.failed;
        view! {
            {c.running.then(|| view! {
                <button
                    class="btn"
                    class:danger=c.abort_next
                    title=cancel_title
                    disabled=move || page.busy.get()
                    on:click=cancel
                >
                    {cancel_text}
                </button>
            })}
            {(c.resumable || c.retry_only).then(|| view! {
                <label class="dl-conc" title="How many items are asked at once; admission and the model's slots are the only other bound">
                    <span class="lbl">"concurrency"</span>
                    <input
                        class="input w-num"
                        type="number"
                        min="1"
                        step="1"
                        prop:value=move || concurrency.get()
                        on:input=move |ev| concurrency.set(event_target_value(&ev))
                    />
                </label>
            })}
            {(c.resumable && failed_n > 0).then(|| view! {
                <label class="dl-check" title="Ask the items whose run failed (no 2xx) again too; each failed run is replaced by the new one">
                    <input
                        type="checkbox"
                        prop:checked=move || retry.get()
                        on:change=move |ev| retry.set(event_target_checked(&ev))
                    />
                    {format!("retry the {} failed", grouped(failed_n))}
                </label>
            })}
            {c.resumable.then(|| view! {
                <button
                    class="btn primary"
                    title="A new job for the items without a run"
                    disabled=move || page.busy.get() || !conc_ok()
                    on:click=move |_| resume(retry.get_untracked())
                >
                    "Resume"
                </button>
            })}
            {c.retry_only.then(|| view! {
                <button
                    class="btn primary"
                    title="A new job for the items whose run failed; each failed run is replaced by the new one"
                    disabled=move || page.busy.get() || !conc_ok()
                    on:click=move |_| resume(true)
                >
                    {format!("Retry the {} failed", grouped(failed_n))}
                </button>
            })}
        }
    };

    view! {
        <div class="card dl-runcard">
            <div class="dl-card-head">
                <h3 class="lab-h3">"Progress"</h3>
                <span class="spacer"></span>
                <span class="mini-note dim">
                    {move || page.view.with(|v| {
                        v.as_ref().map(|v| format!("created {}", when(&v.created_at)))
                    })}
                </span>
            </div>
            {move || view! { <ProgressBar p=progress.get()/> }}
            <p class="dl-prog-line">{move || progress_line(progress.get(), controls.get().running)}</p>
            {move || page.view.with(|v| {
                let v = v.as_ref()?;
                let e = v.job_error.clone()?;
                let lead = match v.status {
                    BatchStatus::Interrupted => "Interrupted: ",
                    _ => "The last job failed: ",
                };
                Some(view! { <div class="notice warn dl-job-error">{lead}{e}</div> })
            })}
            <div class="dl-runctl">
                {buttons}
                <span class="spacer"></span>
                <ConfirmButton
                    label="Delete"
                    confirm=delete_confirm
                    title="Delete this set run, its items, their runs and the files nothing else names"
                    disabled=Signal::derive(move || page.busy.get())
                    on_confirm=delete
                />
            </div>
            {move || page.view.with(|v| v.as_ref().map(facts))}
        </div>
    }
}

/// What the run ran and how it reads the file.
fn facts(v: &BatchView) -> impl IntoView {
    let questions = v
        .set
        .questions
        .iter()
        .map(question_fact)
        .collect::<Vec<_>>()
        .join(" · ");
    let set = match &v.preset_name {
        Some(n) => format!("'{n}' (saved)"),
        None => "unsaved: its thresholds cannot be written".into(),
    };
    view! {
        <dl class="dl-runfacts">
            <dt>"set"</dt>
            <dd>{set}</dd>
            <dt>"questions"</dt>
            <dd>{questions}</dd>
            <dt>"model"</dt>
            <dd class="mono-sm">{v.model.clone()}</dd>
            <dt>"file"</dt>
            <dd>{v.source_name.clone()}</dd>
            <dt>"mapping"</dt>
            <dd class="mono-sm">{mapping_fact(v)}</dd>
            <dt>"labelled"</dt>
            <dd>{format!("{} of {} items", grouped(v.labelled), grouped(v.total))}</dd>
            <dt>"concurrency"</dt>
            <dd>{v.concurrency.to_string()}</dd>
        </dl>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(done: u64, failed: u64, unreadable: u64) -> Progress {
        Progress {
            total: 1000,
            done,
            failed,
            unreadable,
            cancel_requested: false,
        }
    }

    #[test]
    fn the_chip_says_where_the_run_stopped() {
        assert_eq!(
            status_chip(BatchStatus::Interrupted, p(340, 0, 0)).1,
            "interrupted at 340 of 1\u{202F}000"
        );
        assert_eq!(status_chip(BatchStatus::Running, p(1, 0, 0)).0, "chip live");
        assert_eq!(
            status_chip(BatchStatus::Canceled, p(2, 0, 0)).1,
            "canceled at 2 of 1\u{202F}000"
        );
    }

    #[test]
    fn the_progress_line_counts_every_kind_of_run() {
        assert_eq!(
            progress_line(p(34, 2, 1), true),
            "34 of 1\u{202F}000 · 31 answered · 2 failed · 1 unreadable · 966 to go"
        );
        let done = Progress {
            total: 34,
            ..p(34, 0, 0)
        };
        assert_eq!(
            progress_line(done, false),
            "34 of 34 · 34 answered · 0 failed · 0 unreadable"
        );
    }

    #[test]
    fn the_second_cancel_says_it_aborts() {
        assert_eq!(cancel_label(false).0, "Cancel");
        assert!(cancel_label(true).0.contains("Abort the calls in flight"));
    }

    fn view(status: &str, cancel_requested: bool) -> BatchView {
        serde_json::from_value(serde_json::json!({
            "id": 7, "lab": "decision", "preset_id": null, "preset_name": null,
            "set": {"questions": [], "default_model": "", "thresholds": {}},
            "model": "decide/laya", "source_name": "tickets.csv",
            "mapping": {"state": "text"}, "concurrency": 1, "status": status,
            "total": 1000, "done": 0, "failed": 0, "unreadable": 0, "labelled": 0,
            "job_id": 3, "job_error": null, "cancel_requested": cancel_requested,
            "created_at": ""
        }))
        .unwrap()
    }

    #[test]
    fn a_progress_frame_does_not_move_the_controls() {
        let running = view("running", false);
        let a = Controls::of(&running, p(10, 0, 0));
        let b = Controls::of(&running, p(34, 3, 1));
        assert_eq!(a, b, "the frames of one job draw the controls once");
        assert!(a.running && !a.abort_next && !a.resumable);
        // A reloaded page knows a Cancel went out: the next press aborts.
        let asked = Controls::of(&view("running", true), p(34, 3, 1));
        assert!(asked.abort_next);
        assert_ne!(asked, b);
        // A Cancel from another tab reaches the page by a frame alone.
        let framed = Controls::of(
            &running,
            Progress {
                cancel_requested: true,
                ..p(34, 3, 1)
            },
        );
        assert!(framed.abort_next);
        // Stopped: the failures are named, and Resume offers to retry them.
        let stopped = Controls::of(&view("canceled", false), p(34, 3, 1));
        assert_eq!((stopped.resumable, stopped.failed), (true, 3));
        let done = Controls::of(
            &view("done", false),
            Progress {
                total: 34,
                ..p(34, 2, 0)
            },
        );
        assert_eq!((done.retry_only, done.failed), (true, 2));
        assert!(!Controls::of(&view("canceled", true), p(1, 0, 0)).abort_next);
        assert_eq!(
            delete_question(7, 30, true),
            "Delete set run #7, its 30 items and their runs (the calls in flight are aborted)?"
        );
    }
}
