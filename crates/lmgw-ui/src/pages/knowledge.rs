//! Knowledge — the owner's own documents in named collections (chat-complete
//! design §9.5), as a list beside the selected base on the shared split view
//! (`crate::widgets::nav_split`).
//!
//! One route for both, `/knowledge/:id?/:tab?`: `/knowledge` (nothing
//! selected), `/knowledge/<id>` (its Files), `/knowledge/<id>/search` and
//! `/knowledge/<id>/settings`. The list ([`list`]) has the title, New
//! knowledge base and the search (`?q=`, `/`, Ctrl+F, Esc), one row per base
//! with its state; ↑ ↓ Home End move the selection. The detail ([`detail`])
//! has the tabs in its header bar, the hero with the base's state, size and
//! counts, the notes, and the tab. Under 800 px of pane one pane shows at a
//! time, the detail with a back button.
//!
//! The backend is `/api/knowledge/*`; live progress is the ordinary jobs
//! feed (`kb_ingest` / `kb_reembed`, key `kb:<id>`), consumed the way the
//! Docs page consumes its own: the durable rows are re-read when a job
//! starts, ends or finishes a file, and every tick in between arrives on the
//! bus — nothing here polls.

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use lmgw_api_types::JobRow;
use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::scope::Scope;
use crate::widgets::list_pane::{ListRefs, Nav};
use crate::widgets::nav_split::{provide_save_key, NavSplit, SplitConf, SplitState};
use crate::widgets::{use_toasts, Dot};

/// The selected base: hero, tabs, the delete question.
mod detail;
/// The list pane.
mod list;

/// The two job kinds that report against a base; both use the key `kb:<id>`.
pub const KB_JOB_KINDS: [&str; 2] = ["kb_ingest", "kb_reembed"];

// ---------------------------------------------------------------------------
// What the API sends
// ---------------------------------------------------------------------------

pub use lmgw_api_types::knowledge::{
    KnowledgeBase as Kb, KnowledgeBaseDetail as DetailResponse, KnowledgeBaseList as BasesResponse,
    KnowledgeFile as KbFile,
};

// ---------------------------------------------------------------------------
// Live job
// ---------------------------------------------------------------------------

/// The jobs running against the bases right now, of either kind, by base
/// id: one pass over the jobs feed per tick, read by every row and tab.
fn kb_jobs() -> Memo<BTreeMap<i64, JobRow>> {
    let live = crate::live::use_live();
    Memo::new(move |_| {
        live.jobs.with(|js| {
            js.as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|j| KB_JOB_KINDS.contains(&j.kind.as_str()))
                .filter_map(|j| {
                    let id = j.key.as_deref()?.strip_prefix("kb:")?.parse().ok()?;
                    Some((id, j.clone()))
                })
                .collect()
        })
    })
}

/// Bump `reload` whenever `mark` changes after its first read.
fn reload_on<T: Clone + PartialEq + Send + Sync + 'static>(reload: RwSignal<u32>, mark: Memo<T>) {
    Effect::new(move |prev: Option<T>| {
        let now = mark.get();
        if prev.is_some_and(|p| p != now) {
            reload.update(|n| *n += 1);
        }
        now
    });
}

/// The list is read again when a knowledge job starts, ends or changes its
/// stage. A file finished in between is read by the selected base's detail
/// alone ([`reload_on_own_job`]), which puts what it read into the list.
fn reload_on_jobs(page: KbPage) {
    let jobs = page.jobs;
    let marks = Memo::new(move |_| {
        jobs.with(|m| {
            m.values()
                .map(|j| (j.id, j.stage.clone()))
                .collect::<Vec<_>>()
        })
    });
    reload_on(page.reload, marks);
}

/// The base's detail is read again when its own job starts, ends, changes
/// its stage or finishes a file, so the file list follows an ingest one file
/// at a time.
fn reload_on_own_job(page: KbPage, id: i64, reload: RwSignal<u32>) {
    let jobs = page.jobs;
    let mark =
        Memo::new(move |_| jobs.with(|m| m.get(&id).map(|j| (j.id, j.done, j.stage.clone()))));
    reload_on(reload, mark);
}

fn job_unit(kind: &str) -> &'static str {
    if kind == "kb_reembed" {
        "chunks"
    } else {
        "files"
    }
}

fn job_title(kind: &str) -> &'static str {
    if kind == "kb_reembed" {
        "Re-embedding"
    } else {
        "Ingesting"
    }
}

/// Ask the base's job to stop.
fn cancel_job(toasts: crate::widgets::Toasts, id: i64) {
    leptos::task::spawn_local(async move {
        match crate::api::post::<Value, _>(format!("/api/knowledge/bases/{id}/cancel"), &json!({}))
            .await
        {
            Ok(_) => toasts.ok("cancel requested"),
            Err(e) => toasts.err(e.to_string()),
        }
    });
}

/// Start the base's waiting work again.
pub fn resume_kb(toasts: crate::widgets::Toasts, ctx: KbCtx) {
    leptos::task::spawn_local(async move {
        match crate::api::post::<lmgw_api_types::knowledge::ResumeResult, _>(
            format!("/api/knowledge/bases/{}/resume", ctx.id),
            &json!({}),
        )
        .await
        {
            Ok(v) => toasts.ok(v.message.unwrap_or_else(|| "resumed".into())),
            Err(e) => toasts.err(e.to_string()),
        }
        if ctx.scope.alive() {
            ctx.bump();
        }
    });
}

/// The live job as a row of a boxed group: what it is doing and on which
/// file, how far, the bar under it, and Cancel. Nothing while no job runs.
#[component]
pub fn KbJobRow(id: i64) -> impl IntoView {
    let toasts = use_toasts();
    let page = use_kb_page();
    let job = Memo::new(move |_| page.job(id));
    view! {
        {move || {
            job.get()
                .map(|j| {
                    let unit = job_unit(&j.kind);
                    let counts = match j.total {
                        Some(t) => format!("{} / {t} {unit}", j.done),
                        None => format!("{} {unit}", j.done),
                    };
                    let file = j
                        .detail
                        .get("file")
                        .and_then(Value::as_str)
                        .filter(|f| !f.is_empty())
                        .map(|f| {
                            let stage = j
                                .detail
                                .get("file_stage")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if stage.is_empty() { f.to_string() } else { format!("{f} · {stage}") }
                        });
                    view! {
                        <div class="prow stack kb-job">
                            <div class="prow-text">
                                <span class="prow-title">
                                    {job_title(&j.kind)} " · " {j.stage.clone()}
                                </span>
                                {file.map(|f| {
                                    let tip = f.clone();
                                    view! { <span class="prow-sub" title=tip>{f}</span> }
                                })}
                            </div>
                            <div class="prow-end">
                                <span class="prow-value">{counts}</span>
                                <button type="button" class="btn sm" on:click=move |_| cancel_job(toasts, id)>
                                    "Cancel"
                                </button>
                            </div>
                            <div class="progress">
                                <i style=format!("width:{}%", j.percent.unwrap_or(3))></i>
                            </div>
                        </div>
                    }
                })
        }}
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// What the base needs, in a dot and a word: the live job when there is
/// one, else the most pressing thing the counts say. The third value is what
/// a tooltip says beyond the word.
pub fn status_of(kb: &Kb, live: Option<&JobRow>) -> (Dot, String, String) {
    let c = &kb.counts;
    let reembed = kb.status == "re_embed_required" || c.embedded < c.chunks;
    let last_error = kb.job.as_ref().and_then(|j| j.error.clone());
    let last_failed = kb
        .job
        .as_ref()
        .is_some_and(|j| j.status == "failed" && j.error.is_some());
    if let Some(j) = live {
        let word = match j.total {
            Some(t) => format!("{} {}/{t}", job_title(&j.kind), j.done),
            None => job_title(&j.kind).to_string(),
        };
        return (Dot::Busy, word, String::new());
    }
    if last_failed && (c.pending > 0 || reembed) {
        return (
            Dot::Warn,
            "Stopped, Resume".into(),
            last_error.unwrap_or_default(),
        );
    }
    if reembed {
        return (
            Dot::Error,
            "Re-embed required".into(),
            "some chunks have no vector for the pinned model".into(),
        );
    }
    if c.failed > 0 {
        return (Dot::Error, format!("{} failed", c.failed), String::new());
    }
    if c.pending > 0 {
        return (Dot::Warn, format!("{} waiting", c.pending), String::new());
    }
    if c.files == 0 {
        return (Dot::Off, "Empty".into(), String::new());
    }
    (Dot::Running, "Ready".into(), String::new())
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

/// What the list and the detail share: the bases, their reload, the New
/// dialog and the moves (asked about first when they would leave edits).
#[derive(Clone, Copy)]
pub struct KbPage {
    pub bases: RwSignal<Vec<Kb>>,
    pub loaded: RwSignal<bool>,
    pub error: RwSignal<Option<String>>,
    /// Bumped after every mutation; the list's resource tracks it.
    pub reload: RwSignal<u32>,
    pub new_open: RwSignal<bool>,
    /// The jobs running against the bases, by base id.
    pub jobs: Memo<BTreeMap<i64, JobRow>>,
    nav: Nav,
}

impl KbPage {
    pub fn bump(&self) {
        self.reload.update(|n| *n += 1);
    }

    /// The job running against base `id` right now, of either kind. Tracked.
    pub fn job(self, id: i64) -> Option<JobRow> {
        self.jobs.with(|m| m.get(&id).cloned())
    }

    /// Whether a job runs against base `id`. Tracked.
    pub fn has_job(self, id: i64) -> bool {
        self.jobs.with(|m| m.contains_key(&id))
    }

    /// A base as just read by its detail, into the list beside it.
    pub fn put(self, kb: &Kb) {
        let stale = self
            .bases
            .with_untracked(|bs| bs.iter().any(|b| b.id == kb.id && b != kb));
        if stale {
            self.bases.update(|bs| {
                if let Some(b) = bs.iter_mut().find(|b| b.id == kb.id) {
                    *b = kb.clone();
                }
            });
        }
    }

    /// Go to `to`, asking first when unsaved edits would be left behind.
    /// Whether it moved now.
    pub fn go(self, to: &str, replace: bool) -> bool {
        self.nav.go(to, replace)
    }

    /// Go without asking: what would be asked about is gone already.
    pub fn go_unasked(self, to: &str, replace: bool) {
        self.nav.go_unasked(to, replace);
    }

    pub fn guard(self) -> crate::widgets::DirtyGuard {
        self.nav.guard()
    }
}

pub fn use_kb_page() -> KbPage {
    expect_context::<KbPage>()
}

/// What a base's tabs share: the base, its files, and the reload trigger.
#[derive(Clone, Copy)]
pub struct KbCtx {
    pub id: i64,
    pub kb: RwSignal<Option<Kb>>,
    pub files: RwSignal<Vec<KbFile>>,
    pub reload: RwSignal<u32>,
    pub scope: Scope,
    /// The delete question is up.
    pub deleting: RwSignal<bool>,
    pub page: KbPage,
}

impl KbCtx {
    /// Read the base again, and the list beside it.
    pub fn bump(&self) {
        self.reload.update(|n| *n += 1);
        self.page.bump();
    }
}

pub fn use_kb() -> KbCtx {
    expect_context::<KbCtx>()
}

pub const KB_TABS: &[&str] = &["files", "search", "settings"];

pub fn kb_href(id: i64, tab: &str) -> String {
    match tab {
        "search" | "settings" => format!("/knowledge/{id}/{tab}"),
        _ => format!("/knowledge/{id}"),
    }
}

/// The list's own query keys, carried from one selection to the next.
const LIST_KEYS: &[&str] = &["q"];

/// Below 800 px of pane the view is one pane at a time, as Models'.
const CONF: SplitConf = SplitConf {
    class: "kb-split",
    width_key: "knowledge.list.width",
    list_width: (240, 30, 380),
    detail_min: 460,
    narrow_below: 800.0,
    handle_label: "Resize the list",
};

/// `/knowledge/:id?/:tab?`
#[component]
pub fn Knowledge() -> impl IntoView {
    let params = use_params_map();
    // The id segment as written, and as a base id: a segment that is no
    // number selects nothing the list has, and the detail says so.
    let raw = Memo::new(move |_| params.with(|p| p.get_str("id").map(str::to_string)));
    let sel = Memo::new(move |_| raw.get().and_then(|s| s.parse::<i64>().ok()));
    let page = KbPage {
        bases: RwSignal::new(Vec::new()),
        loaded: RwSignal::new(false),
        error: RwSignal::new(None),
        reload: RwSignal::new(0),
        new_open: RwSignal::new(false),
        jobs: kb_jobs(),
        nav: Nav::new(),
    };
    provide_context(page);

    let res = LocalResource::new(move || {
        page.reload.get();
        crate::api::get::<BasesResponse>("/api/knowledge/bases")
    });
    Effect::new(move |_| match res.get() {
        Some(Ok(v)) => {
            page.bases.set(v.bases);
            page.error.set(None);
            page.loaded.set(true);
        }
        // A failed re-read keeps the list it had, and says why.
        Some(Err(e)) => page.error.set(Some(e.to_string())),
        None => {}
    });
    reload_on_jobs(page);

    let split = SplitState::new(CONF);
    let narrow = split.narrow;
    let save_key = provide_save_key();
    let refs = ListRefs::new();
    let has_sel = Signal::derive(move || raw.with(Option::is_some));
    let open_row = Callback::new(move |(id, replace): (i64, bool)| {
        page.nav
            .open(&format!("/knowledge/{id}"), LIST_KEYS, replace);
    });
    let back = Callback::new(move |()| page.nav.back("/knowledge", LIST_KEYS, refs));

    view! {
        <NavSplit
            split=split
            has_sel=has_sel
            save_key=save_key
            search=refs.search
            search_off=Signal::derive(move || narrow.get() && has_sel.get())
            list=ViewFn::from(move || {
                view! { <list::ListPane sel=sel narrow=narrow refs=refs open_row=open_row/> }
            })
            detail=ViewFn::from(move || {
                view! { <detail::DetailPane raw=raw sel=sel narrow=narrow back=back/> }
            })
        />
        <super::knowledge_new::NewKbModal open=page.new_open on_created=Callback::new(move |_| page.bump())/>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb(files: i64, chunks: i64, embedded: i64) -> Kb {
        let mut k = Kb {
            status: "ready".into(),
            ..Default::default()
        };
        k.counts.files = files;
        k.counts.chunks = chunks;
        k.counts.embedded = embedded;
        k
    }

    #[test]
    fn the_state_says_the_most_pressing_thing() {
        assert_eq!(status_of(&kb(0, 0, 0), None).0, Dot::Off);
        assert_eq!(status_of(&kb(2, 10, 10), None).1, "Ready");
        let (dot, word, ..) = status_of(&kb(2, 10, 4), None);
        assert_eq!((dot, word.as_str()), (Dot::Error, "Re-embed required"));
        let mut waiting = kb(3, 10, 10);
        waiting.counts.pending = 2;
        assert_eq!(status_of(&waiting, None).1, "2 waiting");
        let job = JobRow {
            kind: "kb_ingest".into(),
            done: 1,
            total: Some(3),
            ..Default::default()
        };
        assert_eq!(status_of(&waiting, Some(&job)).1, "Ingesting 1/3");
    }
}
