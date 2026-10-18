//! The Decision lab's set runs (labs-store design §3.3), newest first, in
//! the reference panel: each with its set, model and file, its status and
//! its progress, a link to its page. A running one's progress follows the
//! jobs feed; a set-run job starting or ending (and a Clear or a delete,
//! which the usage's set-run count shows) reads the list again.

use leptos::prelude::*;
use leptos_router::components::A;
use lmgw_api_types::decision_lab::batch::JOB_KIND;
use lmgw_api_types::decision_lab::{BatchJobDetail, BatchList, BatchStatus, BatchView};

use super::batch_status::status_chip;
use super::batch_view::{LiveJob, Progress};
use super::page::Lab;
use crate::api;
use crate::fmt::grouped;
use crate::scope::Latest;

/// The lab's set runs. `Copy`, a part of `Lab`.
#[derive(Clone, Copy)]
pub(super) struct Runs {
    /// Newest first; `None` until the first read lands.
    pub list: RwSignal<Option<Result<Vec<BatchView>, String>>>,
    reads: Latest,
}

impl Runs {
    pub fn new() -> Self {
        Self {
            list: RwSignal::new(None),
            reads: Latest::new(),
        }
    }

    /// How many are running, for the panel's pill.
    pub fn running(self) -> usize {
        self.list.with(|l| {
            l.as_ref()
                .and_then(|r| r.as_ref().ok())
                .map(|v| {
                    v.iter()
                        .filter(|b| b.status == BatchStatus::Running)
                        .count()
                })
                .unwrap_or(0)
        })
    }
}

pub(super) fn reload(lab: Lab) {
    let runs = lab.runs;
    let ticket = runs.reads.next();
    lab.scope.spawn(async move {
        let got = api::get::<BatchList>("/decision-lab/api/batches").await;
        if ticket.is_some_and(|t| runs.reads.is(t)) {
            runs.list
                .set(Some(got.map(|l| l.batches).map_err(|e| e.to_string())));
        }
    });
}

/// The set-run jobs live now, by run id: the feed's progress.
fn live_jobs(jobs: &[lmgw_api_types::JobRow]) -> Vec<(i64, LiveJob)> {
    jobs.iter()
        .filter(|j| j.kind == JOB_KIND)
        .filter_map(|j| {
            let detail: BatchJobDetail = serde_json::from_value(j.detail.clone()).ok()?;
            Some((
                detail.batch_id,
                LiveJob {
                    job_id: j.id,
                    detail,
                },
            ))
        })
        .collect()
}

/// Follow the feed: a set-run job starting or ending reads the list again.
pub(super) fn follow(lab: Lab) {
    let bus = crate::live::use_live();
    let keys = Memo::new(move |_| {
        bus.jobs.with(|j| {
            j.as_ref()
                .map(|j| {
                    j.iter()
                        .filter(|r| r.kind == JOB_KIND)
                        .map(|r| r.id)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    Effect::new(move |prev: Option<Vec<i64>>| {
        let now = keys.get();
        if prev.is_some_and(|p| p != now) {
            reload(lab);
        }
        now
    });
    // A Clear or a set run deleted elsewhere moves the usage's count.
    Effect::new(move |prev: Option<Option<u64>>| {
        let now = lab.history.usage.with(|u| u.as_ref().map(|u| u.batches));
        if prev.is_some_and(|p| p.is_some() && p != now) {
            reload(lab);
        }
        now
    });
}

#[component]
pub(super) fn RunsPanel(lab: Lab) -> impl IntoView {
    let bus = crate::live::use_live();
    let live = Memo::new(move |_| {
        bus.jobs
            .with(|j| j.as_ref().map(|j| live_jobs(j)).unwrap_or_default())
    });
    view! {
        <h3 class="lab-h3">"Set runs"</h3>
        {move || match lab.runs.list.get() {
            None => view! { <p class="mini-note dim">"Reading the set runs…"</p> }.into_any(),
            Some(Err(e)) => view! {
                <p class="mini-note bad">
                    {format!("The set runs did not load: {e} ")}
                    <button class="btn ghost sm" on:click=move |_| reload(lab)>"Retry"</button>
                </p>
            }
            .into_any(),
            Some(Ok(v)) if v.is_empty() => view! {
                <p class="mini-note dim">
                    "None yet: New set run asks the form's questions of every row of a CSV or JSONL file."
                </p>
            }
            .into_any(),
            Some(Ok(v)) => {
                let live = live.get();
                v.into_iter()
                    .map(|b| {
                        let l = live.iter().find(|(id, _)| *id == b.id).map(|(_, l)| l.clone());
                        let p = Progress::of(&b, l.as_ref());
                        view! { <RunRow b=b p=p/> }
                    })
                    .collect_view()
                    .into_any()
            }
        }}
    }
}

#[component]
fn RunRow(b: BatchView, p: Progress) -> impl IntoView {
    let (class, text) = status_chip(b.status, p);
    let set = b
        .preset_name
        .clone()
        .unwrap_or_else(|| "unsaved set".into());
    let total = p.total.max(1) as f64;
    let mut line = format!("{} of {}", grouped(p.done), grouped(p.total));
    if p.failed > 0 {
        line.push_str(&format!(" · {} failed", grouped(p.failed)));
    }
    if b.labelled > 0 {
        line.push_str(" · labelled");
    }
    view! {
        <A href=format!("/decision-lab/batches/{}", b.id) attr:class="dl-runrow">
            <span class="dl-runrow-head">
                <b class="n">{format!("#{} · {set}", b.id)}</b>
                <span class=format!("{class} sm")>{text}</span>
            </span>
            <span class="mini-note dim dl-runrow-sub" title=format!("{} · {}", b.model, b.source_name)>
                {format!("{} · {}", b.model, b.source_name)}
            </span>
            <span class="mini-note dl-runrow-prog">{line}</span>
            <span class="dl-track">
                <i style=format!("width:{:.2}%", p.done as f64 / total * 100.0)></i>
            </span>
        </A>
    }
}
