//! Backends — container images built from git (container-builds design
//! §9.1). A **build** is the saved definition (repository, ref, extras merged
//! on top, GPU backend, build args); a **run** is one execution of it, a
//! background job on the shared jobs feed (`kind = "build_run"`, key
//! `build:<id>`). The Builds tab lists the definitions with their run
//! history; the Images tab lists every local image of the three engines,
//! built here or not.
//!
//! The durable lists are re-read when a run starts or ends (the set of live
//! `build_run` job ids changes), never on a timer: progress in between rides
//! the jobs feed, and a run's log is polled only while its log says it is
//! not done.
//!
//! All ops are the §15 contract, through [`crate::backends_api`].

use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::builds::{
    moving_tag, BuildEnv, BuildExtra, BuildRun, BuildRunJobDetail, BuildRunStatus, BuildView,
    ContainerImagesResponse, Engine, ImagePullJobDetail, ImageUse, ImageUseKind, UpdateStatus,
    UpdatesSummary,
};
use lmgw_api_types::JobRow;

use crate::backends_api as api;
use crate::scope::{Latest, Scope};
use crate::widgets::{use_toasts, NavTab, PageFrame, PageMode, SubNav, Toasts, Tone};

mod builds;
mod editor;
mod extras;
mod images;
mod pull;
mod recreate;
mod reports;
mod run_log;

pub use editor::EditorSeed;
pub use pull::PullTarget;
pub use run_log::LogTarget;

pub const TABS: &[&str] = &["builds", "images"];

/// Where a tab lives.
pub fn href(tab: &str) -> String {
    match tab {
        "images" => "/backends/images".to_string(),
        _ => "/backends".to_string(),
    }
}

/// The jobs feed's kind for a build run (§15 "Jobs feed").
pub const RUN_KIND: &str = "build_run";

/// One live run as the jobs feed reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveRun {
    pub job_id: i64,
    pub detail: BuildRunJobDetail,
    /// The whole run's progress: the job row's percent, which climbs across
    /// podman's stages and never falls back. The detail's own `percent` is
    /// only the compile inside the current step.
    pub percent: Option<u64>,
}

/// Every live run on the feed, by build id. The key (`build:<id>`) names the
/// build; the detail's `build_id` is the fallback for a row whose key is
/// missing.
pub fn live_runs(jobs: &[JobRow]) -> HashMap<i64, LiveRun> {
    jobs.iter()
        .filter(|j| j.kind == RUN_KIND)
        .filter_map(|j| {
            let detail: BuildRunJobDetail =
                serde_json::from_value(j.detail.clone()).unwrap_or_default();
            let build_id = j
                .key
                .as_deref()
                .and_then(|k| k.strip_prefix("build:"))
                .and_then(|k| k.parse::<i64>().ok())
                .or((detail.build_id != 0).then_some(detail.build_id))?;
            let percent = j.percent;
            Some((
                build_id,
                LiveRun {
                    job_id: j.id,
                    detail,
                    percent,
                },
            ))
        })
        .collect()
}

/// The jobs feed's kind for a registry pull (§8 **Pull update**).
pub const PULL_KIND: &str = "image_pull";

/// One live pull on the feed.
#[derive(Clone, Debug, PartialEq)]
pub struct LivePull {
    pub job_id: i64,
    pub detail: ImagePullJobDetail,
}

/// Every live pull on the feed, by the reference it pulls. The key
/// (`image:<ref>`) names it; the detail's `image` is the fallback.
pub fn live_pulls(jobs: &[JobRow]) -> HashMap<String, LivePull> {
    jobs.iter()
        .filter(|j| j.kind == PULL_KIND)
        .filter_map(|j| {
            let detail: ImagePullJobDetail =
                serde_json::from_value(j.detail.clone()).unwrap_or_default();
            let image = j
                .key
                .as_deref()
                .and_then(|k| k.strip_prefix("image:"))
                .map(str::to_string)
                .or_else(|| (!detail.image.is_empty()).then(|| detail.image.clone()))?;
            Some((
                image,
                LivePull {
                    job_id: j.id,
                    detail,
                },
            ))
        })
        .collect()
}

/// The nav badge's tooltip: what the update count is made of, and when it
/// was last looked for.
pub fn updates_title(u: &UpdatesSummary) -> String {
    let mut parts = Vec::new();
    if u.builds_with_updates > 0 {
        parts.push(format!(
            "{} with newer inputs than {} last verified run",
            crate::fmt::count_of(u.builds_with_updates as usize, "builds"),
            if u.builds_with_updates == 1 {
                "its"
            } else {
                "their"
            }
        ));
    }
    if u.images_with_updates > 0 {
        parts.push(format!(
            "{} in use with a newer version on the registry",
            crate::fmt::count_of(u.images_with_updates as usize, "images"),
        ));
    }
    let mut t = if parts.is_empty() {
        "no updates".to_string()
    } else {
        format!("Updates: {}", parts.join("; "))
    };
    if let Some(at) = u.checked_at.as_deref() {
        t.push_str(&format!(" (checked {})", ago(at)));
    }
    t
}

/// How one build's update check reads in a toast: `(worth a warning, text)`.
/// An update and a failed check are; "up to date" and "nothing to compare
/// with" are not.
pub fn update_outcome(name: &str, u: Option<&UpdateStatus>) -> (bool, String) {
    match u {
        None => (
            false,
            format!("{name}: nothing to compare with yet — a verified run comes first"),
        ),
        Some(u) if u.has_update() => (true, format!("{name}: {}", u.reasons.join(" · "))),
        Some(u) if !u.errors.is_empty() => (true, format!("{name}: {}", u.errors.join(" · "))),
        Some(_) => (false, format!("{name}: up to date")),
    }
}

/// Page state every tab, row and modal reaches. `Copy`: rows capture it at
/// render, handlers run without an owner to look context up in.
#[derive(Clone, Copy)]
pub struct Bk {
    pub toasts: Toasts,
    pub scope: Scope,
    pub builds: RwSignal<Option<Vec<BuildView>>>,
    pub builds_err: RwSignal<Option<String>>,
    /// Who uses which image could not be told (podman did not answer): every
    /// "used by" on the page is empty for that reason, and the page says so.
    pub usage_err: RwSignal<Option<String>>,
    /// Build ids with a **Check now** in flight; `0` is the global one.
    pub checking: RwSignal<HashSet<i64>>,
    /// Bumped on every successful builds read: expanded run histories
    /// re-read their first page on it.
    pub rev: RwSignal<u64>,
    pub images: RwSignal<Option<ContainerImagesResponse>>,
    pub images_err: RwSignal<Option<String>>,
    /// The Images tab has been shown once: from then on its list is kept
    /// current along with the builds.
    pub images_wanted: RwSignal<bool>,
    /// A refusal from an image op, shown above the table until dismissed —
    /// the delete refusal lists the image's users, which a toast would
    /// squeeze.
    pub images_notice: RwSignal<Option<String>>,
    pub env: RwSignal<Option<Result<BuildEnv, String>>>,
    env_asked: RwSignal<bool>,
    /// Build ids whose run history is unfolded.
    pub expanded: RwSignal<HashSet<i64>>,
    pub editor: RwSignal<Option<EditorSeed>>,
    pub log: RwSignal<Option<LogTarget>>,
    /// Row "Check merge": `(build id, name)`.
    pub merge: RwSignal<Option<(i64, String)>>,
    pub deleting: RwSignal<Option<BuildView>>,
    /// Retag: `(image id, its tags)`.
    pub retag: RwSignal<Option<(String, Vec<String>)>>,
    /// The image ID the delete confirmation is open for — an image in use,
    /// or one deleted from a run's menu.
    pub image_delete: RwSignal<Option<String>>,
    /// Live runs by build id, off the shared jobs feed.
    pub live: Memo<HashMap<i64, LiveRun>>,
    /// Live registry pulls by reference, off the same feed.
    pub pulls: Memo<HashMap<String, LivePull>>,
    /// The Pull update panel: which reference, and its job once started.
    pub pull: RwSignal<Option<PullTarget>>,
    /// Pulls started from this page visit, by reference → job id: a row
    /// whose pull finished while its panel was closed can open the result
    /// (and its recreate offer) again.
    pub pulled: RwSignal<HashMap<String, i64>>,
    /// The newest builds / images read: an older answer landing late is
    /// dropped instead of putting back what a newer one replaced.
    builds_read: Latest,
    images_read: Latest,
}

impl Bk {
    /// Read the builds again (and the images, once their tab was shown).
    ///
    /// Safe from any continuation: an op that toasts its outcome after the
    /// page was left calls this too, and there is nothing to load then — the
    /// page's signals are gone, and reading one would trap the module.
    pub fn load_builds(self) {
        if !self.scope.alive() {
            return;
        }
        let Some(ticket) = self.builds_read.next() else {
            return;
        };
        self.scope.spawn(async move {
            let res = api::builds().await;
            if !self.builds_read.is(ticket) {
                return;
            }
            match res {
                Ok(r) => {
                    self.builds_err.set(None);
                    self.usage_err.set(r.usage_error);
                    self.builds.set(Some(r.builds));
                    self.rev.update(|n| *n += 1);
                }
                Err(e) => self.builds_err.set(Some(e.to_string())),
            }
        });
        if self.images_wanted.try_get_untracked().unwrap_or(false) {
            self.load_images();
        }
    }

    /// Read the images again; from now on they follow the builds. Safe from
    /// any continuation, like [`Bk::load_builds`].
    pub fn load_images(self) {
        if !self.scope.alive() {
            return;
        }
        let Some(ticket) = self.images_read.next() else {
            return;
        };
        self.images_wanted.set(true);
        self.scope.spawn(async move {
            let res = api::container_images(true).await;
            if !self.images_read.is(ticket) {
                return;
            }
            match res {
                Ok(r) => {
                    self.images_err.set(None);
                    self.images.set(Some(r));
                }
                Err(e) => self.images_err.set(Some(e.to_string())),
            }
        });
    }

    /// Fetch `build_env` once per page visit: the editor and the empty
    /// state's presets both need it.
    pub fn ensure_env(self) {
        if self.env_asked.try_get_untracked().unwrap_or(true) {
            return;
        }
        self.env_asked.set(true);
        self.scope.spawn(async move {
            let res = api::build_env().await.map_err(|e| e.to_string());
            self.env.set(Some(res));
        });
    }

    /// Re-ask for `build_env` (the Retry on its failure notice).
    pub fn retry_env(self) {
        self.env_asked.set(false);
        self.env.set(None);
        self.ensure_env();
    }

    /// Start a run and open its log.
    pub fn run(self, id: i64, name: String, rebuild: bool) {
        spawn_local(async move {
            match api::build_run(id, rebuild).await {
                Ok(s) => {
                    self.toasts.ok(if rebuild {
                        format!("{name}: rebuild started")
                    } else {
                        format!("{name}: run started")
                    });
                    if !self.scope.alive() {
                        return;
                    }
                    self.log.set(Some(LogTarget {
                        run_id: s.run_id,
                        build_id: Some(id),
                        name,
                        promoted: None,
                    }));
                    self.load_builds();
                }
                Err(e) => self.toasts.err(format!("{name}: {e}")),
            }
        });
    }

    /// Open the log of a build's live run, else of its last run.
    pub fn open_log(self, v: &BuildView) {
        let live_run = self
            .live
            .with_untracked(|m| m.get(&v.build.id).map(|l| l.detail.run_id));
        let run_id = live_run
            .filter(|id| *id != 0)
            .or_else(|| v.last_run.as_ref().map(|r| r.id));
        match run_id {
            Some(run_id) => self.log.set(Some(LogTarget {
                run_id,
                build_id: Some(v.build.id),
                name: v.build.spec.name.clone(),
                promoted: None,
            })),
            None => self.toasts.warn("this build has not run yet"),
        }
    }

    /// **Check now** (§8): one build, or (`None`) every build and every
    /// registry image in use. The badges come back re-read with the lists.
    pub fn check_updates(self, id: Option<i64>, name: String) {
        let slot = id.unwrap_or(0);
        if self.checking.with_untracked(|c| c.contains(&slot)) {
            return;
        }
        self.checking.update(|c| {
            c.insert(slot);
        });
        spawn_local(async move {
            let res = api::build_updates_check(id).await;
            self.checking.try_update(|c| {
                c.remove(&slot);
            });
            match res {
                Ok(r) => {
                    if id.is_some() {
                        let u = r.updates.first().and_then(|e| e.update.as_ref());
                        let (warn, msg) = update_outcome(&name, u);
                        if warn {
                            self.toasts.warn(msg);
                        } else {
                            self.toasts.ok(msg);
                        }
                    } else {
                        let n = r.updates.len();
                        let with = r
                            .updates
                            .iter()
                            .filter(|e| e.update.as_ref().is_some_and(UpdateStatus::has_update))
                            .count();
                        let failed = r
                            .updates
                            .iter()
                            .filter(|e| e.update.as_ref().is_some_and(|u| !u.errors.is_empty()))
                            .count();
                        let mut msg = format!(
                            "checked {}: {}",
                            crate::fmt::count_of(n, "builds"),
                            match with {
                                0 => "no updates".to_string(),
                                w => format!("{w} with updates"),
                            }
                        );
                        if failed > 0 {
                            msg.push_str(&format!(", {failed} could not be checked"));
                        }
                        msg.push_str(" — registry images are on the Images tab");
                        if failed > 0 {
                            self.toasts.warn(msg);
                        } else {
                            self.toasts.ok(msg);
                        }
                    }
                    // The images' registry badges come along: load_builds
                    // re-reads the Images tab too once it has been shown.
                    if self.scope.alive() {
                        self.load_builds();
                    }
                }
                Err(e) => self.toasts.err(format!("{name}: {e}")),
            }
        });
    }

    pub fn cancel(self, job_id: i64, name: String) {
        spawn_local(async move {
            match api::job_cancel(job_id).await {
                Ok(_) => self.toasts.ok(format!("{name}: cancel requested")),
                Err(e) => self.toasts.err(format!("{name}: {e}")),
            }
        });
    }
}

pub fn use_bk() -> Bk {
    expect_context::<Bk>()
}

#[component]
pub fn Backends() -> impl IntoView {
    let tab = crate::url_state::use_view("tab", TABS, "builds");
    let bus = crate::live::use_live();
    let live = Memo::new(move |_| {
        bus.jobs
            .with(|j| live_runs(j.as_deref().unwrap_or_default()))
    });
    let pulls = Memo::new(move |_| {
        bus.jobs
            .with(|j| live_pulls(j.as_deref().unwrap_or_default()))
    });
    let bk = Bk {
        toasts: use_toasts(),
        scope: Scope::new(),
        builds: RwSignal::new(None),
        builds_err: RwSignal::new(None),
        usage_err: RwSignal::new(None),
        checking: RwSignal::new(HashSet::new()),
        rev: RwSignal::new(0),
        images: RwSignal::new(None),
        images_err: RwSignal::new(None),
        images_wanted: RwSignal::new(false),
        images_notice: RwSignal::new(None),
        env: RwSignal::new(None),
        env_asked: RwSignal::new(false),
        expanded: RwSignal::new(HashSet::new()),
        editor: RwSignal::new(None),
        log: RwSignal::new(None),
        merge: RwSignal::new(None),
        deleting: RwSignal::new(None),
        retag: RwSignal::new(None),
        image_delete: RwSignal::new(None),
        live,
        pulls,
        pull: RwSignal::new(None),
        pulled: RwSignal::new(HashMap::new()),
        builds_read: Latest::new(),
        images_read: Latest::new(),
    };
    provide_context(bk);
    bk.load_builds();

    // A run starting or ending is when the durable rows change.
    let run_jobs = Memo::new(move |_| {
        let mut ids: Vec<i64> = live.with(|m| m.values().map(|l| l.job_id).collect());
        ids.sort_unstable();
        ids
    });
    Effect::new(move |prev: Option<Vec<i64>>| {
        let now = run_jobs.get();
        if prev.is_some_and(|p| p != now) {
            bk.load_builds();
        }
        now
    });
    // A pull ending changes the Images tab (the new image, its update chip).
    let pull_jobs = Memo::new(move |_| {
        let mut ids: Vec<i64> = pulls.with(|m| m.values().map(|l| l.job_id).collect());
        ids.sort_unstable();
        ids
    });
    Effect::new(move |prev: Option<Vec<i64>>| {
        let now = pull_jobs.get();
        if prev.is_some_and(|p| p != now) && bk.images_wanted.get_untracked() {
            bk.load_images();
        }
        now
    });
    // The update counts moved (a check ended elsewhere, the schedule ran): the
    // badges on the rows are re-read with the lists.
    Effect::new(move |prev: Option<Option<UpdatesSummary>>| {
        let now = bus.updates.get();
        if prev.is_some_and(|p| p.is_some() && p != now) {
            bk.load_builds();
        }
        now
    });
    // The Images tab reads its list the first time it is shown.
    Effect::new(move |_| {
        if tab.get() == "images" && !bk.images_wanted.get_untracked() {
            bk.load_images();
        }
    });

    let tabs = Signal::derive(move || {
        let mut b = NavTab::new("Builds", href("builds")).exact();
        if let Some(n) = bk.builds.with(|b| b.as_ref().map(Vec::len)) {
            b = b.count(n);
        }
        let running = live.with(HashMap::len);
        if running > 0 {
            b = b.count(format!("{running} running")).tone(Tone::Attn);
        }
        let upd = bus
            .updates
            .with(|u| u.as_ref().map_or(0, |u| u.builds_with_updates));
        if upd > 0 && running == 0 {
            b = b
                .count(format!("{upd} update{}", if upd == 1 { "" } else { "s" }))
                .tone(Tone::Attn);
        }
        let mut i = NavTab::new("Images", href("images"));
        if let Some(n) = bk.images.with(|r| r.as_ref().map(|r| r.images.len())) {
            i = i.count(n);
        }
        let img_upd = bus
            .updates
            .with(|u| u.as_ref().map_or(0, |u| u.images_with_updates));
        let pulling = pulls.with(HashMap::len);
        if pulling > 0 {
            i = i.count(format!("{pulling} pulling")).tone(Tone::Attn);
        } else if img_upd > 0 {
            i = i
                .count(format!(
                    "{img_upd} update{}",
                    if img_upd == 1 { "" } else { "s" }
                ))
                .tone(Tone::Attn);
        }
        vec![b, i]
    });
    let summary = move || {
        let Some(n) = bk.builds.with(|b| b.as_ref().map(Vec::len)) else {
            return "container images built from git".to_string();
        };
        let running = live.with(HashMap::len);
        let builds = crate::fmt::count_of(n, "builds");
        if running > 0 {
            format!("{builds} · {running} running")
        } else {
            builds
        }
    };
    let refresh = move |_| {
        bk.load_builds();
        if tab.get_untracked() == "images" {
            bk.load_images();
        }
    };
    let check_all = move |_| bk.check_updates(None, "update check".to_string());
    let checking_all = move || bk.checking.with(|c| c.contains(&0));
    let check_title = move || {
        let last = bus
            .updates
            .with(|u| u.as_ref().and_then(|u| u.checked_at.clone()))
            .map(|at| format!("Last full check {} ({}).", ago(&at), local_ts(&at)))
            .unwrap_or_else(|| "No full check has run yet.".to_string());
        format!(
            "Ask the remotes now: every build's ref and unpinned extras, and the registry images in use. {last} The schedule is in Settings → Backends."
        )
    };
    let new_build = move |_| {
        bk.ensure_env();
        bk.editor.set(Some(EditorSeed::blank()));
    };

    view! {
        <PageFrame
            title="Backends"
            sub=summary
            mode=PageMode::Fill
            class="backends"
            head_extra=move || view! { <SubNav tabs=tabs/> }
            actions=move || {
                view! {
                    <button
                        class="btn ghost"
                        title="Read the builds and the local images again"
                        on:click=refresh
                    >
                        "Refresh"
                    </button>
                    <button
                        class="btn ghost"
                        title=check_title
                        disabled=checking_all
                        on:click=check_all
                    >
                        {move || if checking_all() { "Checking…" } else { "Check now" }}
                    </button>
                    <button class="btn primary" on:click=new_build>
                        "New build"
                    </button>
                }
            }
        >
            {move || match tab.get() {
                "images" => view! { <images::ImagesTab/> }.into_any(),
                _ => view! { <builds::BuildsTab/> }.into_any(),
            }}
            <editor::BuildEditorModal/>
            <run_log::RunLogModal/>
            <builds::MergeModal/>
            <builds::DeleteBuildModal/>
            <images::RetagModal/>
            <images::ImageDeleteModal/>
            <pull::PullModal/>
        </PageFrame>
    }
}

// ---------------------------------------------------------------------------
// Shared display helpers
// ---------------------------------------------------------------------------

pub fn engine_label(e: Engine) -> &'static str {
    match e {
        Engine::Llama => "llama.cpp",
        Engine::Audio => "audio.cpp",
        Engine::Sdcpp => "sd.cpp",
    }
}

pub fn engine_chip(e: Engine) -> impl IntoView {
    let title = format!("serves: {}", e.classes().join(", "));
    view! { <span class="type-badge bk-engine" title=title>{engine_label(e)}</span> }
}

/// `https://github.com/ggml-org/llama.cpp.git` → `ggml-org/llama.cpp`; a
/// `git@host:path` address → `path`; anything else as it is.
pub fn short_repo(url: &str) -> String {
    let u = url.trim();
    let rest = match u.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map_or(rest, |(_, p)| p),
        None => match u.split_once(':') {
            Some((host, p)) if host.contains('@') => p,
            _ => u,
        },
    };
    let rest = rest.trim_end_matches('/');
    rest.strip_suffix(".git").unwrap_or(rest).to_string()
}

/// The first seven characters of a SHA.
pub fn sha7(s: &str) -> String {
    s.chars().take(7).collect()
}

/// How an extra reads in a chip: `#16391`, `fork/llama.cpp feature`.
pub fn extra_short(e: &BuildExtra) -> String {
    match e {
        BuildExtra::Pr { number, .. } => format!("#{number}"),
        BuildExtra::Ref {
            remote_url,
            git_ref,
            ..
        } => {
            let r = git_ref
                .strip_prefix("refs/heads/")
                .or_else(|| git_ref.strip_prefix("refs/tags/"))
                .unwrap_or(git_ref);
            format!("{} {r}", short_repo(remote_url))
        }
    }
}

/// One user of an image, in words.
pub fn use_label(u: &ImageUse) -> String {
    match u.kind {
        ImageUseKind::ClassDefault => format!("{} default", u.class),
        ImageUseKind::ModelOverride => {
            format!("model {}", u.model_id.as_deref().unwrap_or("(unnamed)"))
        }
        ImageUseKind::RunningContainer => format!(
            "running: {}",
            u.container.as_deref().unwrap_or("(container)")
        ),
        ImageUseKind::StoppedContainer => format!(
            "stopped: {}",
            u.container.as_deref().unwrap_or("(container)")
        ),
    }
}

pub fn use_chips(uses: &[ImageUse]) -> impl IntoView {
    uses.iter()
        .map(|u| {
            let title = format!("class {}", u.class);
            view! { <span class="type-badge" title=title>{use_label(u)}</span> }
        })
        .collect_view()
}

/// Where a dev instance tags (§10): each engine's repository has a twin in
/// lmgw's dev namespace, `localhost/lmgw-dev-llama-server`, beside
/// production's `localhost/lmgw-llama-server`. Both are lmgw's builds.
pub const DEV_REPO_PREFIX: &str = "localhost/lmgw-dev-";

/// `engine`'s repository in the dev namespace.
pub fn dev_repo(engine: Engine) -> String {
    let repo = engine.image_repo();
    let name = repo.strip_prefix("localhost/lmgw-").unwrap_or(repo);
    format!("{DEV_REPO_PREFIX}{name}")
}

/// Is the reference in the dev namespace?
pub fn is_dev_tag(tag: &str) -> bool {
    tag.trim().starts_with(DEV_REPO_PREFIX)
}

/// Is `tag` the moving tag of build `slug` — production's spelling or a dev
/// instance's?
pub fn is_moving_tag(tag: &str, engine: Engine, slug: &str) -> bool {
    tag == moving_tag(engine, slug) || tag == format!("{}:{slug}", dev_repo(engine))
}

/// The run id of a `<immutable>-r<run id>` tag: every run tags its image so
/// first, and one that neither verified nor was the first image of its
/// inputs (an unverified **Rebuild anyway**) keeps only that tag.
pub fn rebuild_tag_run(tag: &str) -> Option<i64> {
    let (rest, id) = tag_only(tag).rsplit_once("-r")?;
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
    let (rest, cfg) = rest.rsplit_once('-')?;
    let (_, base) = rest.rsplit_once('-')?;
    (hex(cfg, 6) && hex(base, 7) && !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        .then(|| id.parse().ok())
        .flatten()
}

/// A run's own tag: the immutable tag it earned when it has it, else its
/// `-r<run>` tag (see [`rebuild_tag_run`]). Never a moving tag, in either
/// namespace.
pub fn run_tag(run: &BuildRun) -> Option<String> {
    let own: Vec<&String> = run
        .tags
        .iter()
        .filter(|t| !is_moving_tag(t, run.engine, &run.slug))
        .collect();
    own.iter()
        .find(|t| rebuild_tag_run(t).is_none())
        .or_else(|| own.first())
        .map(|t| t.to_string())
}

/// The moving tag a run's build has on the instance that built it: the one
/// among its tags when it holds it; the dev twin when its tags are in the dev
/// namespace; production's otherwise.
pub fn run_moving_tag(run: &BuildRun) -> String {
    if let Some(t) = run
        .tags
        .iter()
        .find(|t| is_moving_tag(t, run.engine, &run.slug))
    {
        return t.clone();
    }
    if run.tags.iter().any(|t| is_dev_tag(t)) {
        format!("{}:{}", dev_repo(run.engine), run.slug)
    } else {
        moving_tag(run.engine, &run.slug)
    }
}

/// A tag without its repository: `official-master-abc1234-def567`.
pub fn tag_only(tag: &str) -> &str {
    match tag.rsplit_once(':') {
        Some((repo, t)) if !t.contains('/') && !repo.is_empty() => t,
        _ => tag,
    }
}

/// Unix seconds from a timestamp as the server writes them: SQLite's UTC
/// `YYYY-MM-DD HH:MM:SS`, RFC 3339 (with `Z` or an offset, optional
/// fraction), or bare unix seconds (podman's `Created`).
pub fn parse_ts(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse::<f64>().ok();
    }
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let rest = rest.trim_start();
    let offset = match rest.chars().next() {
        None | Some('Z') | Some('z') => 0,
        Some(sign @ ('+' | '-')) => {
            let digits: String = rest[1..]
                .chars()
                .filter(|c| c.is_ascii_digit())
                .take(4)
                .collect();
            if digits.len() != 4 {
                return None;
            }
            let oh: i64 = digits[..2].parse().ok()?;
            let om: i64 = digits[2..].parse().ok()?;
            let o = oh * 3600 + om * 60;
            if sign == '+' {
                o
            } else {
                -o
            }
        }
        _ => 0,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60 + se - offset) as f64)
}

/// "4m ago", from a server timestamp; the raw value when it does not parse.
pub fn ago(ts: &str) -> String {
    match parse_ts(ts) {
        Some(t) => crate::fmt::rel_time(t),
        None => ts.to_string(),
    }
}

/// The local date and time of a server timestamp, for a tooltip.
pub fn local_ts(ts: &str) -> String {
    match parse_ts(ts) {
        Some(t) => crate::fmt::local_datetime(t),
        None => ts.to_string(),
    }
}

/// "5m 50s", "1h 02m", "45s".
pub fn fmt_duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// How long a run took, or has been going.
pub fn run_duration(run: &BuildRun) -> Option<String> {
    let start = parse_ts(&run.started_at)?;
    let end = match run.finished_at.as_deref() {
        Some(f) => parse_ts(f)?,
        None => js_sys::Date::now() / 1000.0,
    };
    Some(fmt_duration((end - start).max(0.0) as u64))
}

pub fn status_label(s: BuildRunStatus) -> &'static str {
    match s {
        BuildRunStatus::Running => "running",
        BuildRunStatus::Succeeded => "succeeded",
        BuildRunStatus::Unverified => "unverified",
        BuildRunStatus::Broken => "broken",
        BuildRunStatus::Failed => "failed",
        BuildRunStatus::Canceled => "canceled",
        BuildRunStatus::UpToDate => "up to date",
    }
}

fn status_class(s: BuildRunStatus) -> &'static str {
    match s {
        BuildRunStatus::Running => "chip live",
        BuildRunStatus::Succeeded | BuildRunStatus::UpToDate => "chip ok",
        BuildRunStatus::Unverified => "chip warn",
        BuildRunStatus::Broken | BuildRunStatus::Failed => "chip err",
        BuildRunStatus::Canceled => "chip off",
    }
}

fn status_title(s: BuildRunStatus) -> &'static str {
    match s {
        BuildRunStatus::Running => "the run is in progress",
        BuildRunStatus::Succeeded => "built, GPU-verified and made current",
        BuildRunStatus::Unverified => {
            "built, but not GPU-verified (GPU hold, or out of memory while probing) — Verify now can finish it"
        }
        BuildRunStatus::Broken => "built, but a verify check failed — not made current",
        BuildRunStatus::Failed => "did not produce an image",
        BuildRunStatus::Canceled => "canceled before it produced an image",
        BuildRunStatus::UpToDate => {
            "an image for exactly these inputs already existed and was verified; nothing was built"
        }
    }
}

/// A run's status chip. `live` is the feed's view of it while it runs: the
/// phase and the percent say more than "running".
pub fn status_chip(status: BuildRunStatus, live: Option<&LiveRun>) -> AnyView {
    if let Some(l) = live {
        let label = match (l.detail.phase, l.percent) {
            (lmgw_api_types::builds::BuildPhase::Waiting, _) => "waiting".to_string(),
            (_, Some(p)) => format!("{} {p}%", phase_label(l.detail.phase)),
            (p, None) => phase_label(p).to_string(),
        };
        let title = match &l.detail.waiting_for {
            Some(w) => format!("waiting for {w} (one build runs at a time)"),
            None => l.detail.last_line.clone(),
        };
        return view! {
            <span class="chip live" title=title><span class="dot"></span>{label}</span>
        }
        .into_any();
    }
    view! {
        <span class=status_class(status) title=status_title(status)>
            <span class="dot"></span>
            {status_label(status)}
        </span>
    }
    .into_any()
}

pub fn phase_label(p: lmgw_api_types::builds::BuildPhase) -> &'static str {
    use lmgw_api_types::builds::BuildPhase as P;
    match p {
        P::Waiting => "waiting",
        P::Resolve => "resolve",
        P::Fetch => "fetch",
        P::Assemble => "assemble",
        P::Prepare => "prepare",
        P::Build => "build",
        P::Verify => "verify",
        P::Promote => "promote",
        P::Cleanup => "cleanup",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn job(id: i64, kind: &str, key: Option<&str>, detail: serde_json::Value) -> JobRow {
        JobRow {
            id,
            kind: kind.into(),
            key: key.map(String::from),
            label: String::new(),
            status: "running".into(),
            done: 0,
            total: None,
            percent: Some(7),
            stage: String::new(),
            detail,
            error: None,
            created_at: String::new(),
            started_at: None,
            finished_at: None,
        }
    }

    #[test]
    fn live_runs_are_keyed_by_build_and_ignore_other_jobs() {
        let jobs = vec![
            job(
                1,
                RUN_KIND,
                Some("build:3"),
                json!({"run_id": 9, "build_id": 3, "phase": "build", "percent": 45}),
            ),
            job(2, "hf_download", Some("hf:3"), json!({})),
            // no key: the detail names the build
            job(
                3,
                RUN_KIND,
                None,
                json!({"run_id": 10, "build_id": 4, "phase": "fetch"}),
            ),
        ];
        let m = live_runs(&jobs);
        assert_eq!(m.len(), 2);
        assert_eq!(m[&3].job_id, 1);
        assert_eq!(m[&3].detail.run_id, 9);
        // the bar is the job row's whole-run percent; the detail's is the
        // compile inside the current step
        assert_eq!(m[&3].percent, Some(7));
        assert_eq!(m[&3].detail.percent, Some(45));
        assert_eq!(m[&4].percent, Some(7));
    }

    #[test]
    fn live_pulls_are_keyed_by_the_reference_they_pull() {
        let jobs = vec![
            job(
                5,
                PULL_KIND,
                Some("image:ghcr.io/o/audio.cpp:full-cuda12"),
                json!({"image": "ghcr.io/o/audio.cpp:full-cuda12", "last_line": "Copying blob"}),
            ),
            job(
                6,
                PULL_KIND,
                None,
                json!({"image": "docker.io/library/alpine:latest"}),
            ),
            job(7, PULL_KIND, None, json!({})),
            job(8, RUN_KIND, Some("build:1"), json!({})),
        ];
        let m = live_pulls(&jobs);
        assert_eq!(m.len(), 2);
        assert_eq!(m["ghcr.io/o/audio.cpp:full-cuda12"].job_id, 5);
        assert_eq!(
            m["ghcr.io/o/audio.cpp:full-cuda12"].detail.last_line,
            "Copying blob"
        );
        assert_eq!(m["docker.io/library/alpine:latest"].job_id, 6);
    }

    #[test]
    fn the_update_badge_says_what_it_counts() {
        let u = UpdatesSummary {
            builds_with_updates: 1,
            images_with_updates: 2,
            checked_at: None,
        };
        assert_eq!(
            updates_title(&u),
            "Updates: 1 build with newer inputs than its last verified run; 2 images in use with a newer version on the registry"
        );
        let u = UpdatesSummary {
            builds_with_updates: 3,
            ..Default::default()
        };
        assert!(updates_title(&u).contains("3 builds with newer inputs than their"));
        assert_eq!(updates_title(&UpdatesSummary::default()), "no updates");
    }

    #[test]
    fn a_check_outcome_warns_on_updates_and_failures_only() {
        assert!(!update_outcome("b", None).0);
        let up = UpdateStatus::default();
        assert_eq!(
            update_outcome("b", Some(&up)),
            (false, "b: up to date".to_string())
        );
        let failed = UpdateStatus {
            errors: vec!["check failed: rate limited until 12:00".into()],
            ..Default::default()
        };
        assert!(update_outcome("b", Some(&failed)).0);
        let moved = UpdateStatus {
            reasons: vec!["master +37 commits".into()],
            errors: vec!["check failed: x".into()],
            ..Default::default()
        };
        assert_eq!(
            update_outcome("b", Some(&moved)),
            (true, "b: master +37 commits".to_string())
        );
    }

    #[test]
    fn repos_read_short() {
        assert_eq!(
            short_repo("https://github.com/ggml-org/llama.cpp.git"),
            "ggml-org/llama.cpp"
        );
        assert_eq!(
            short_repo("git@git.example.com:alice/lmgw.git"),
            "alice/lmgw"
        );
        assert_eq!(short_repo("ssh://git@host/a/b/"), "a/b");
        assert_eq!(short_repo("/srv/repo"), "/srv/repo");
    }

    #[test]
    fn extras_read_as_chips() {
        assert_eq!(
            extra_short(&BuildExtra::Pr {
                number: 16391,
                pin: None
            }),
            "#16391"
        );
        assert_eq!(
            extra_short(&BuildExtra::Ref {
                remote_url: "https://github.com/fork/llama.cpp".into(),
                git_ref: "refs/heads/feature-x".into(),
                pin: None,
            }),
            "fork/llama.cpp feature-x"
        );
    }

    #[test]
    fn timestamps_parse_in_every_shape_the_server_writes() {
        // SQLite's datetime('now'), UTC
        assert_eq!(parse_ts("1970-01-02 00:00:00"), Some(86_400.0));
        assert_eq!(parse_ts("2026-09-26 00:00:00"), Some(1_790_380_800.0));
        // RFC 3339, Z and offsets, with a fraction
        assert_eq!(parse_ts("2026-09-26T00:00:00Z"), Some(1_790_380_800.0));
        assert_eq!(
            parse_ts("2026-09-26T02:00:00.123+02:00"),
            Some(1_790_380_800.0)
        );
        // podman's CreatedAt, and its unix Created
        assert_eq!(
            parse_ts("2026-09-26 02:00:00 +0200 CEST"),
            Some(1_790_380_800.0)
        );
        assert_eq!(parse_ts("1790380800"), Some(1_790_380_800.0));
        assert_eq!(parse_ts("yesterday"), None);
        assert_eq!(parse_ts(""), None);
    }

    #[test]
    fn durations_read_short() {
        assert_eq!(fmt_duration(45), "45s");
        assert_eq!(fmt_duration(350), "5m 50s");
        assert_eq!(fmt_duration(3720), "1h 02m");
    }

    #[test]
    fn dev_builds_are_lmgw_builds_in_their_own_namespace() {
        assert_eq!(dev_repo(Engine::Llama), "localhost/lmgw-dev-llama-server");
        assert_eq!(dev_repo(Engine::Sdcpp), "localhost/lmgw-dev-sd-server");
        let e = Engine::Llama;
        assert!(is_moving_tag("localhost/lmgw-llama-server:om", e, "om"));
        assert!(is_moving_tag("localhost/lmgw-dev-llama-server:om", e, "om"));
        assert!(!is_moving_tag(
            "localhost/lmgw-dev-llama-server:om-abc1234-def567",
            e,
            "om"
        ));
        assert!(!is_moving_tag("localhost/lmgw-audio-cpp:om", e, "om"));
        assert!(is_dev_tag("localhost/lmgw-dev-audio-cpp:x"));
        assert!(!is_dev_tag("localhost/lmgw-audio-cpp:x"));
    }

    #[test]
    fn a_rebuild_tag_names_its_run_and_nothing_else_does() {
        assert_eq!(
            rebuild_tag_run("localhost/lmgw-llama-server:om-abc1234-def567-r42"),
            Some(42)
        );
        assert_eq!(
            rebuild_tag_run("localhost/lmgw-llama-server:om-abc1234-def567"),
            None
        );
        // a slug ending in -r2 is not a run tag
        assert_eq!(rebuild_tag_run("localhost/lmgw-llama-server:exp-r2"), None);
        assert_eq!(
            rebuild_tag_run("localhost/lmgw-llama-server:x-abc1234-def567-r"),
            None
        );
        assert_eq!(rebuild_tag_run("x-abcxyz1-def567-r3"), None);
    }

    #[test]
    fn a_runs_own_tag_is_its_immutable_one_else_its_rebuild_tag() {
        let run = |tags: &[&str]| BuildRun {
            slug: "om".into(),
            engine: Engine::Llama,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..Default::default()
        };
        let imm = "localhost/lmgw-llama-server:om-abc1234-def567";
        let r = "localhost/lmgw-llama-server:om-abc1234-def567-r7";
        let moving = "localhost/lmgw-llama-server:om";
        // verified and promoted: the immutable tag, whatever the order
        assert_eq!(run_tag(&run(&[r, moving, imm])).as_deref(), Some(imm));
        // an unverified rebuild keeps only its -r tag
        assert_eq!(run_tag(&run(&[r])).as_deref(), Some(r));
        // a dev instance's moving tag is not a run's own tag either
        let dev = run(&[
            "localhost/lmgw-dev-llama-server:om",
            "localhost/lmgw-dev-llama-server:om-abc1234-def567",
        ]);
        assert_eq!(
            run_tag(&dev).as_deref(),
            Some("localhost/lmgw-dev-llama-server:om-abc1234-def567")
        );
        assert_eq!(run_moving_tag(&dev), "localhost/lmgw-dev-llama-server:om");
        assert_eq!(
            run_moving_tag(&run(&[
                "localhost/lmgw-dev-llama-server:om-abc1234-def567-r1"
            ])),
            "localhost/lmgw-dev-llama-server:om"
        );
        assert_eq!(run_moving_tag(&run(&[imm])), moving);
        assert_eq!(run_tag(&run(&[moving])), None);
    }

    #[test]
    fn a_tag_without_its_repository() {
        assert_eq!(
            tag_only("localhost/lmgw-llama-server:official-master"),
            "official-master"
        );
        assert_eq!(tag_only("localhost:5000/img"), "localhost:5000/img");
    }
}
