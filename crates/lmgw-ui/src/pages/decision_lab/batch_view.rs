//! A set run's page (labs-store design §3.3, §3.4): `/decision-lab/batches/:id`.
//! Its status and progress, followed live from the jobs feed (kind
//! `lab_batch`, key `lab-batch:<id>`, detail `BatchJobDetail`) between reads
//! of the run; Cancel, Resume and Delete; the metrics when labels exist;
//! the items table in pages; the export of the run's items.
//!
//! The feed is the trigger, the GETs are the pipe: a progress frame re-reads
//! the items shown (one read in flight at a time), and the job leaving the
//! feed (how a consumer learns it ended) re-reads the run, its items and
//! its metrics.

use leptos::prelude::*;
use leptos_router::components::A;
use leptos_router::hooks::use_params_map;
use lmgw_api_types::decision_lab::batch::JOB_KIND;
use lmgw_api_types::decision_lab::{job_key, BatchJobDetail, BatchView};
use lmgw_api_types::lab_store::{LabExportFormat, LabKind, LabPresetView};

use super::super::lab_history::{export_href, ExportScope};
use super::batch_items::{Items, ItemsCard};
use super::batch_status::{RunHead, StatusCard};
use super::metrics_view::{Metrics, MetricsSection};
use crate::api;
use crate::pages::docs::download;
use crate::scope::{Latest, Scope};
use crate::widgets::{use_toasts, MenuItem, PageFrame, RowMenu, Toasts};

/// The jobs feed's row of this run's job while it runs.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct LiveJob {
    pub job_id: i64,
    pub detail: BatchJobDetail,
}

/// Where the run stands: the live job's counts while it runs, else the
/// run's own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Progress {
    pub total: u64,
    pub done: u64,
    pub failed: u64,
    pub unreadable: u64,
    /// A Cancel was asked of the job, by anyone: the run's own word or the
    /// live job's, whichever has heard of it.
    pub cancel_requested: bool,
}

impl Progress {
    pub fn of(view: &BatchView, live: Option<&LiveJob>) -> Self {
        match live {
            // A job's first frame comes before its first report: its detail
            // is empty then, and the run's own counts stand.
            Some(l) if l.detail.batch_id == view.id => Self {
                total: l.detail.total.max(view.total),
                done: l.detail.done,
                failed: l.detail.failed,
                unreadable: l.detail.unreadable,
                cancel_requested: view.cancel_requested || l.detail.cancel_requested,
            },
            _ => Self {
                total: view.total,
                done: view.done,
                failed: view.failed,
                unreadable: view.unreadable,
                cancel_requested: view.cancel_requested,
            },
        }
    }

    /// The runs that read as an answer: neither failed nor unreadable.
    pub fn answered(self) -> u64 {
        self.done.saturating_sub(self.failed + self.unreadable)
    }
}

/// The saved set the run ran, as it is now: what "Use as threshold" writes
/// into, and the thresholds it holds.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum PresetRead {
    /// The run ran an unsaved set.
    Unsaved,
    Reading,
    Read(LabPresetView),
    /// The set the run ran is deleted since (a 404).
    Deleted,
    /// The read failed: said with Retry.
    Failed(String),
}

impl PresetRead {
    pub fn view(&self) -> Option<&LabPresetView> {
        match self {
            Self::Read(p) => Some(p),
            _ => None,
        }
    }

    /// Why a threshold cannot be written into it now; `None` when it can.
    pub fn why_not(&self) -> Option<String> {
        match self {
            Self::Read(_) => None,
            Self::Unsaved => {
                Some("an unsaved set: save the set and run it to measure a threshold".into())
            }
            Self::Deleted => Some(
                "the set this run ran is deleted: save the set and run it to measure a threshold"
                    .into(),
            ),
            Self::Reading => Some("reading the set…".into()),
            Self::Failed(e) => Some(format!("the set did not read: {e}")),
        }
    }
}

/// The page's state. `Copy`: every child takes it whole.
#[derive(Clone, Copy)]
pub(super) struct RunPage {
    pub id: i64,
    pub view: RwSignal<Option<BatchView>>,
    /// Why the run did not read (a 404 for a deleted one).
    pub failed: RwSignal<Option<String>>,
    /// The run's job in the jobs feed, while it runs.
    pub live: Memo<Option<LiveJob>>,
    /// The saved set the run ran, as it is now (its thresholds).
    pub preset: RwSignal<PresetRead>,
    pub items: Items,
    pub metrics: Metrics,
    /// A control's call is on its way.
    pub busy: RwSignal<bool>,
    pub scope: Scope,
    pub toasts: Toasts,
    reads: Latest,
    preset_reads: Latest,
}

impl RunPage {
    pub fn progress(self) -> Option<Progress> {
        let live = self.live.get();
        self.view
            .with(|v| v.as_ref().map(|v| Progress::of(v, live.as_ref())))
    }

    /// Read the run again (and its saved set, for the thresholds).
    pub fn refresh(self) {
        let ticket = self.reads.next();
        let id = self.id;
        self.scope.spawn(async move {
            let got = api::get::<BatchView>(format!("/decision-lab/api/batches/{id}")).await;
            if !ticket.is_some_and(|t| self.reads.is(t)) {
                return;
            }
            match got {
                Ok(v) => {
                    let preset = v.preset_id;
                    self.view.set(Some(v));
                    self.failed.set(None);
                    match preset {
                        Some(pid) => self.read_preset(pid),
                        None => {
                            self.preset_reads.next();
                            self.preset.set(PresetRead::Unsaved);
                        }
                    }
                }
                Err(e) => self.failed.set(Some(e.to_string())),
            }
        });
    }

    /// Read the saved set `pid` (ticketed: an older read that lands late
    /// is dropped). A 404 is a deleted set; any other failure is said with
    /// Retry. A set already read stays shown while it is read again.
    pub fn read_preset(self, pid: i64) {
        let ticket = self.preset_reads.next();
        if self.preset.with_untracked(|p| p.view().is_none()) {
            self.preset.set(PresetRead::Reading);
        }
        self.scope.spawn(async move {
            let got = api::get::<LabPresetView>(format!("/labs/api/decision/presets/{pid}")).await;
            if !ticket.is_some_and(|t| self.preset_reads.is(t)) {
                return;
            }
            self.preset.set(match got {
                Ok(p) => PresetRead::Read(p),
                Err(e) if e.is_not_found() => PresetRead::Deleted,
                Err(e) => PresetRead::Failed(e.to_string()),
            });
        });
    }

    /// A threshold write's answer: the set as it is after it.
    pub fn took_preset(self, p: LabPresetView) {
        self.preset_reads.next();
        self.preset.set(PresetRead::Read(p));
    }

    /// A control's call (Cancel, Resume) about to go out: its ticket among
    /// the run's reads, taken when it is sent, not when it is answered. A
    /// Cancel can end its job within milliseconds (an abort, or the last
    /// call in flight landing just then), and the job leaving the feed
    /// re-reads the run: that read went out after the call, so its answer
    /// is the newer one, whichever of the two lands first.
    pub fn control(self) -> Option<u64> {
        self.reads.next()
    }

    /// A control's answer, `ticket` from [`control`](Self::control): the
    /// run as it is after the call, unless a read sent since is newer. A
    /// read sent before the call, still in flight, is dropped by the same
    /// ticket.
    pub fn take(self, ticket: Option<u64>, v: BatchView) {
        if !ticket.is_some_and(|t| self.reads.is(t)) {
            return;
        }
        self.view.set(Some(v));
        self.failed.set(None);
    }
}

/// The export's entries for set run `id`.
pub(super) fn export_items(id: i64) -> Vec<(String, String)> {
    [
        ("Items as CSV", LabExportFormat::Csv),
        ("Items as JSONL", LabExportFormat::Jsonl),
    ]
    .into_iter()
    .map(|(label, f)| {
        (
            label.to_string(),
            export_href(LabKind::Decision, f, &ExportScope::Batch(id)),
        )
    })
    .collect()
}

/// The route: the id in the path is the page's identity, so another one is
/// a new page.
#[component]
pub fn DecisionBatch() -> impl IntoView {
    let params = use_params_map();
    move || {
        let id = params.with(|p| p.get_str("id").and_then(|s| s.parse::<i64>().ok()));
        match id {
            Some(id) => view! { <BatchPage id=id/> }.into_any(),
            None => view! {
                <PageFrame title="Set run" sub="no such set run">
                    <div class="card">
                        "That is not a set run's id. " <A href="/decision-lab">"The Decision lab"</A>
                    </div>
                </PageFrame>
            }
            .into_any(),
        }
    }
}

#[component]
fn BatchPage(id: i64) -> impl IntoView {
    let scope = Scope::new();
    let bus = crate::live::use_live();
    let key = job_key(id);
    let live = Memo::new(move |_| {
        bus.jobs.with(|jobs| {
            jobs.as_ref().and_then(|jobs| {
                jobs.iter()
                    .find(|j| j.kind == JOB_KIND && j.key.as_deref() == Some(key.as_str()))
                    .map(|j| LiveJob {
                        job_id: j.id,
                        detail: serde_json::from_value(j.detail.clone()).unwrap_or_default(),
                    })
            })
        })
    });
    let page = RunPage {
        id,
        view: RwSignal::new(None),
        failed: RwSignal::new(None),
        live,
        preset: RwSignal::new(PresetRead::Reading),
        items: Items::new(id, scope),
        metrics: Metrics::new(id, scope),
        busy: RwSignal::new(false),
        scope,
        toasts: use_toasts(),
        reads: Latest::new(),
        preset_reads: Latest::new(),
    };
    page.refresh();
    page.items.first();

    // The feed: a job appearing (a Resume from another window) re-reads the
    // run; its progress re-reads the items that may have changed (a retried
    // item replaces its failed run without moving `done`, so any change of
    // the counts is a frame); its leaving re-reads the run, every item shown
    // and the metrics.
    Effect::new(move |prev: Option<Option<LiveJob>>| {
        let now = live.get();
        if let Some(prev) = prev {
            match (&prev, &now) {
                (None, Some(_)) => page.refresh(),
                (Some(a), Some(b)) if a.job_id != b.job_id => {
                    page.refresh();
                    page.items.refresh();
                }
                (Some(a), Some(b)) if a.detail != b.detail => {
                    let concurrency = page
                        .view
                        .with_untracked(|v| v.as_ref().map_or(1, |v| v.concurrency));
                    page.items.on_frame(
                        b.detail.retry_failed,
                        concurrency,
                        [b.detail.done, b.detail.failed, b.detail.unreadable],
                    );
                }
                (Some(_), None) => {
                    page.refresh();
                    page.items.refresh();
                    page.metrics.reload();
                }
                _ => {}
            }
        }
        now
    });

    let title = format!("Set run #{id}");
    let sub = move || {
        page.view
            .with(|v| v.as_ref().map(sub_line))
            .unwrap_or_default()
    };
    let menu = Signal::derive(move || {
        export_items(id)
            .into_iter()
            .map(|(label, href)| MenuItem::new(label, move || download(&href, "")))
            .collect::<Vec<_>>()
    });
    view! {
        <PageFrame
            title=title
            sub=sub
            class="dl-run-page"
            head_extra=move || view! { <RunHead page=page/> }
            actions=move || {
                view! {
                    <A href="/decision-lab" attr:class="btn ghost">
                        "Decision lab"
                    </A>
                    <span class="dl-run-export">
                        <RowMenu
                            items=menu
                            title="Export this run's items as a file"
                            label=Signal::derive(|| "Export ▾".to_string())
                        />
                    </span>
                }
            }
        >
            {move || {
                page.failed
                    .get()
                    .map(|e| {
                        view! {
                            <div class="notice err">
                                {format!("⚠ Set run #{id} did not read: {e}")}
                                " "
                                <button class="btn ghost sm" on:click=move |_| page.refresh()>
                                    "Retry"
                                </button>
                            </div>
                        }
                    })
            }}
            <Show when=move || page.view.with(Option::is_some)>
                <StatusCard page=page/>
                <MetricsSection page=page/>
                <ItemsCard page=page/>
            </Show>
        </PageFrame>
    }
}

/// The page's subtitle: the set, the model and the file.
pub(super) fn sub_line(v: &BatchView) -> String {
    let set = match &v.preset_name {
        Some(n) => format!("set '{n}'"),
        None => "an unsaved set".to_string(),
    };
    format!("{set} on {} · {}", v.model, v.source_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(done: u64) -> BatchView {
        serde_json::from_value(serde_json::json!({
            "id": 7, "lab": "decision", "preset_id": null, "preset_name": null,
            "set": {"questions": [], "default_model": "", "thresholds": {}},
            "model": "decide/laya", "source_name": "tickets.csv",
            "mapping": {"state": "text"}, "concurrency": 1, "status": "running",
            "total": 30, "done": done, "failed": 1, "unreadable": 0, "labelled": 30,
            "job_id": 3, "job_error": null, "cancel_requested": false, "created_at": ""
        }))
        .unwrap()
    }

    #[test]
    fn progress_follows_the_live_job_once_it_reported() {
        let v = view(4);
        assert_eq!(Progress::of(&v, None).done, 4);
        let early = LiveJob {
            job_id: 3,
            detail: BatchJobDetail::default(),
        };
        assert_eq!(Progress::of(&v, Some(&early)).done, 4, "no report yet");
        let live = LiveJob {
            job_id: 3,
            detail: BatchJobDetail {
                batch_id: 7,
                total: 30,
                done: 12,
                failed: 2,
                unreadable: 1,
                not_kept: 0,
                retry_failed: false,
                cancel_requested: true,
            },
        };
        let p = Progress::of(&v, Some(&live));
        assert_eq!((p.done, p.failed, p.answered()), (12, 2, 9));
        assert!(
            p.cancel_requested,
            "a frame knows a Cancel the view never heard of"
        );
    }

    #[test]
    fn a_preset_says_why_a_threshold_cannot_be_written() {
        assert!(PresetRead::Unsaved.why_not().unwrap().contains("unsaved"));
        assert!(PresetRead::Deleted.why_not().unwrap().contains("deleted"));
        assert_eq!(
            PresetRead::Failed("connection failed: reset".into())
                .why_not()
                .unwrap(),
            "the set did not read: connection failed: reset"
        );
        assert!(PresetRead::Reading.why_not().is_some());
    }

    #[test]
    fn the_export_names_the_run() {
        let e = export_items(7);
        assert_eq!(e[0].1, "/labs/api/decision/export?format=csv&batch=7");
        assert_eq!(e[1].1, "/labs/api/decision/export?format=jsonl&batch=7");
        assert_eq!(
            sub_line(&view(0)),
            "an unsaved set on decide/laya · tickets.csv"
        );
    }
}
