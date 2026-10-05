//! Downloads — every tracked Hugging Face file, grouped by the repo it came
//! from, with live progress off the shared jobs feed (§9c), update tracking
//! (ETag drift → one-click re-download), and the recovery path for wizard
//! downloads that finished after the window closed ("Plan & create").
//!
//! The rows are the durable `hf_models` records; the bytes moving through
//! them are `hf_download` jobs on the `/api/events` `jobs` frame, so the list
//! is only re-fetched when a transfer starts or ends. What each file *is* and
//! who uses it comes from two other reads: the configured models (what a row
//! names as its weights, projector or drafter) and the models dirs' listings
//! (`/api/gguf-files`: size, users, the filename's role guess, and whether
//! the file is still on disk at all).

use std::collections::{BTreeMap, HashMap};

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{
    DownloadRow, DownloadsView, GgufFile, GgufFiles, JobRow, ModelsFull, PlanResult,
};
use serde_json::{json, Value};

mod revision;

use super::wizard::file_role;
use crate::catalog::use_model_catalog;
use crate::fmt::grouped;
use crate::widgets::{
    filter_words, use_toasts, Facet, FacetSet, FilterBar, GroupRow, MenuItem, Modal, ModalFooter,
    PageFrame, PageMode, RowMenu, Toasts,
};

/// The four container targets, in the order the page lists them.
const TARGETS: [&str; 4] = ["chat", "aux", "audio", "image"];
const COLS: u32 = 7;

/// Jobs feed key of the transfer for one tracked download row.
fn job_key(id: i64) -> String {
    format!("hf:{id}")
}

fn urlenc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

/// "644.2 MiB" back to bytes, for a file the listing no longer has; `None`
/// when the size was never known.
fn parse_size(s: &str) -> Option<u64> {
    let (num, unit) = s.trim().split_once(' ')?;
    let n: f64 = num.parse().ok()?;
    let mult = match unit {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// Bytes in the units the server reports file sizes in ("15.9 GiB").
fn human_bin(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// The server's filename guess, for a file its listing does not have (it is
/// gone from disk): the same ladder `classify_repo_file` climbs.
fn name_guess(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if !name.ends_with(".gguf") {
        return "other";
    }
    if name.contains("mmproj") {
        return "mmproj";
    }
    // `mtp` only as a prefix: `…-NEO-MTP-IQ4_XS` is a full model with its own
    // MTP layers. Plain `eagle` is also a model family.
    let prefix = ["mtp-", "mtp_"];
    let anywhere = ["dflash", "dspark", "draft", "eagle3", "eagle-3"];
    if prefix.iter().any(|m| name.starts_with(m)) || anywhere.iter().any(|m| name.contains(m)) {
        return "drafter";
    }
    "weights"
}

/// A target's own primary role: what it downloads on its own rather than as
/// a companion. Every image file is picked deliberately.
fn is_primary(target: &str, role: &str) -> bool {
    target == "image" || role == "weights"
}

/// One tracked file, with what the page knows about it, flattened for the
/// table.
#[derive(Clone, PartialEq, Eq, Hash)]
struct FileItem {
    id: i64,
    repo: String,
    file: String,
    /// `repo/file`: the path under the target's models dir.
    rel: String,
    target: String,
    status: String,
    error: Option<String>,
    size: String,
    size_bytes: Option<u64>,
    /// What it is: the role a configured model gives it, else the
    /// filename's guess.
    role: String,
    /// The role came from a model that uses it, not from the filename.
    role_known: bool,
    used_by: Vec<String>,
    /// `None` when the target's listing did not load: nothing is claimed.
    on_disk: Option<bool>,
    /// A finished chat weights file no model serves yet.
    planable: bool,
    companion: bool,
    /// The revision it was asked at and the commit it came from: the short
    /// text beside the name, the sentence for its title
    /// ([`revision::revision_note`]).
    revision: (String, String),
}

impl FileItem {
    fn matches(&self, words: &[String]) -> bool {
        if words.is_empty() {
            return true;
        }
        let hay = format!(
            "{} {} {} {}",
            self.rel,
            self.role,
            self.target,
            self.used_by.join(" ")
        )
        .to_lowercase();
        words.iter().all(|w| hay.contains(w.as_str()))
    }

    fn in_flight(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "downloading")
    }

    fn facet(&self, f: &str, running: bool) -> bool {
        match f {
            "" => true,
            "downloading" => running || self.in_flight(),
            "failed" => self.status == "failed" || (self.in_flight() && !running),
            "updates" => self.status == "update_available",
            "unused" => self.used_by.is_empty() && self.on_disk != Some(false),
            "missing" => self.on_disk == Some(false),
            t => self.target == t,
        }
    }
}

/// Which configured model uses a path, and as what.
fn uses_of(m: &ModelsFull) -> HashMap<String, (String, Vec<String>)> {
    let mut out: HashMap<String, (String, Vec<String>)> = HashMap::new();
    let mut add = |path: &str, role: &str, who: &str| {
        if path.is_empty() {
            return;
        }
        let e = out
            .entry(path.to_string())
            .or_insert_with(|| (role.to_string(), Vec::new()));
        e.1.push(who.to_string());
    };
    for v in &m.local {
        let l = &v.model;
        add(&l.gguf_path, "weights", &l.model_id);
        if let Some(p) = &l.params.mmproj_path {
            add(p, "mmproj", &l.model_id);
        }
        if let Some(p) = &l.params.draft_gguf_path {
            add(p, "drafter", &l.model_id);
        }
    }
    for v in &m.aux {
        add(&v.model.gguf_path, "weights", &v.model.model_id);
    }
    for v in &m.image {
        for (role, p) in &v.model.files {
            if let Some(p) = p.as_str() {
                add(p, role, &v.model.model_id);
            }
        }
    }
    out
}

/// One repo's files, in table order: what it downloads on its own first,
/// then the companions hanging off it. Or, `singles`, every repo that has
/// one file: a band of its own over each of those was two rows per file,
/// and a file row out of any band would read as the band above's (review
/// ux:U-19). Those rows name their repo instead.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Group {
    repo: String,
    singles: bool,
    files: Vec<FileItem>,
    /// Before the filter.
    total: usize,
    bytes: u64,
    users: Vec<String>,
}

/// The page's reads, refetched into place: a status change updates rows,
/// it never rebuilds the page under the owner's scroll position.
#[derive(Clone, Copy)]
struct DlData {
    rows: RwSignal<Option<Vec<DownloadRow>>>,
    error: RwSignal<Option<String>>,
    models: RwSignal<Option<ModelsFull>>,
    /// Per target: the models dir's listing, or why it did not load.
    disk: RwSignal<HashMap<&'static str, Result<Vec<GgufFile>, String>>>,
    generation: StoredValue<u64>,
}

impl DlData {
    fn new() -> Self {
        let d = Self {
            rows: RwSignal::new(None),
            error: RwSignal::new(None),
            models: RwSignal::new(None),
            disk: RwSignal::new(HashMap::new()),
            generation: StoredValue::new(0),
        };
        d.load();
        d
    }

    fn load(&self) {
        let this = *self;
        // Called from an op's continuation too, which can land after the
        // page is gone: then there is nothing to load into.
        let Some(gen) = this.generation.try_get_value().map(|g| g + 1) else {
            return;
        };
        this.generation.set_value(gen);
        let current = move || this.generation.try_get_value() == Some(gen);
        spawn_local(async move {
            let res = crate::api::get::<DownloadsView>("/api/hf/downloads").await;
            if !current() {
                return;
            }
            match res {
                Ok(v) => {
                    this.rows.set(Some(v.downloads));
                    this.error.set(None);
                }
                Err(e) => this.error.set(Some(e.to_string())),
            }
        });
        spawn_local(async move {
            if let Ok(m) = crate::api::get::<ModelsFull>("/api/models/full").await {
                if current() {
                    this.models.set(Some(m));
                }
            }
        });
        for t in TARGETS {
            spawn_local(async move {
                let res = crate::api::get::<GgufFiles>(format!("/api/gguf-files?target={t}")).await;
                if current() {
                    this.disk.update(|d| {
                        d.insert(t, res.map(|g| g.files).map_err(|e| e.to_string()));
                    });
                }
            });
        }
    }
}

#[component]
pub fn Downloads() -> impl IntoView {
    let toasts = use_toasts();
    let catalog = use_model_catalog();
    let data = DlData::new();
    let confirm = RwSignal::new(None::<FileItem>);

    // Re-fetch the durable rows when a transfer starts or ends; the bytes in
    // between arrive on the feed, so there is nothing to poll for.
    let live = crate::live::use_live();
    let job_ids = Memo::new(move |_| {
        let mut ids: Vec<i64> = live
            .jobs
            .get()
            .unwrap_or_default()
            .iter()
            .filter(|j| j.kind == "hf_download")
            .map(|j| j.id)
            .collect();
        ids.sort_unstable();
        ids
    });
    Effect::new(move |prev: Option<Vec<i64>>| {
        let now = job_ids.get();
        if prev.is_some_and(|p| p != now) {
            data.load();
        }
        now
    });
    // Rows with a transfer running right now, by download id.
    let running = Memo::new(move |_| {
        live.jobs.with(|j| {
            j.as_deref()
                .unwrap_or_default()
                .iter()
                .filter_map(|j| j.key.as_deref()?.strip_prefix("hf:")?.parse::<i64>().ok())
                .collect::<Vec<_>>()
        })
    });

    let items = Memo::new(move |_| {
        let Some(rows) = data.rows.get() else {
            return Vec::new();
        };
        let uses = data
            .models
            .with(|m| m.as_ref().map(uses_of).unwrap_or_default());
        let served = |rel: &str| uses.get(rel).is_some_and(|(r, _)| r != "other");
        data.disk.with(|disk| {
            rows.into_iter()
                .map(|d| {
                    let rel = format!("{}/{}", d.repo, d.file);
                    let listing = disk.get(d.target.as_str());
                    let listed = listing
                        .and_then(|l| l.as_ref().ok())
                        .and_then(|l| l.iter().find(|f| f.path == rel));
                    let on_disk = match listing {
                        Some(Ok(_)) => Some(listed.is_some()),
                        _ => None,
                    };
                    let (role, role_known) = match uses.get(&rel) {
                        Some((r, _)) => (r.clone(), true),
                        None => {
                            let guess =
                                listed.map_or_else(|| name_guess(&rel), |f| f.role_guess.as_str());
                            (file_role(&rel, guess).to_string(), false)
                        }
                    };
                    let used_by = listed
                        .map(|f| f.used_by.clone())
                        .filter(|u| !u.is_empty())
                        .or_else(|| uses.get(&rel).map(|(_, who)| who.clone()))
                        .unwrap_or_default();
                    let planable = d.target == "chat"
                        && d.status == "done"
                        && rel.to_lowercase().ends_with(".gguf")
                        && role == "weights"
                        && !served(&rel)
                        && on_disk != Some(false);
                    let revision = revision::revision_note(&d);
                    FileItem {
                        revision,
                        id: d.id,
                        size_bytes: listed
                            .map(|f| f.size_bytes)
                            .or_else(|| d.size.as_deref().and_then(parse_size)),
                        size: d.size.clone().unwrap_or_default(),
                        companion: !is_primary(&d.target, &role),
                        repo: d.repo,
                        file: d.file,
                        target: d.target,
                        status: d.status,
                        error: d.error,
                        role,
                        role_known,
                        used_by,
                        on_disk,
                        planable,
                        rel,
                    }
                })
                .collect::<Vec<_>>()
        })
    });

    let query = crate::url_state::use_query_signal("q");
    let facet = crate::url_state::use_query_signal("show");
    let narrowed = Signal::derive(move || {
        !query.with(|q| q.trim().is_empty()) || !facet.with(String::is_empty)
    });

    let groups = Memo::new(move |_| {
        let words = filter_words(&query.get());
        let f = facet.get();
        let run = running.get();
        let mut by_repo: BTreeMap<String, (Vec<FileItem>, usize)> = BTreeMap::new();
        items.with(|all| {
            for it in all {
                let e = by_repo
                    .entry(it.repo.clone())
                    .or_insert_with(|| (Vec::new(), 0));
                e.1 += 1;
                if it.matches(&words) && it.facet(&f, run.contains(&it.id)) {
                    e.0.push(it.clone());
                }
            }
        });
        let group = |repo: String, singles: bool, mut files: Vec<FileItem>, total: usize| {
            files.sort_by(|a, b| {
                let key = |f: &FileItem| {
                    (
                        singles.then(|| f.repo.to_lowercase()),
                        f.companion,
                        f.role.clone(),
                        f.file.clone(),
                    )
                };
                key(a).cmp(&key(b))
            });
            let bytes = files.iter().filter_map(|f| f.size_bytes).sum();
            let mut users: Vec<String> = files.iter().flat_map(|f| f.used_by.clone()).collect();
            users.sort();
            users.dedup();
            Group {
                repo,
                singles,
                files,
                total,
                bytes,
                users,
            }
        };
        let (mut singles, mut singles_total) = (Vec::new(), 0);
        let mut out: Vec<Group> = Vec::new();
        for (repo, (files, total)) in by_repo {
            if total == 1 {
                singles_total += 1;
                singles.extend(files);
            } else if !files.is_empty() {
                out.push(group(repo, false, files, total));
            }
        }
        out.sort_by_key(|g| g.repo.to_lowercase());
        if !singles.is_empty() {
            out.push(group(String::new(), true, singles, singles_total));
        }
        out
    });
    let shown =
        Signal::derive(move || groups.with(|g| g.iter().map(|g| g.files.len()).sum::<usize>()));
    let total = Signal::derive(move || items.with(Vec::len));

    let facets = Signal::derive(move || {
        let run = running.get();
        let active = facet.get();
        items.with(|all| {
            let count = |id: &str| {
                all.iter()
                    .filter(|i| i.facet(id, run.contains(&i.id)))
                    .count()
            };
            [
                ("chat", "Chat"),
                ("aux", "Aux"),
                ("audio", "Audio"),
                ("image", "Image"),
            ]
            .into_iter()
            .chain([
                ("downloading", "Downloading"),
                ("failed", "Failed"),
                ("updates", "Updates"),
                ("unused", "Unused"),
                ("missing", "Missing"),
            ])
            .map(|(id, label)| Facet {
                id: id.into(),
                label: label.into(),
                count: count(id),
            })
            // An empty facet is only noise; the summary line says the
            // zeros. One that is on stays, so it can be seen and cleared.
            .filter(|f| f.count > 0 || f.id == active)
            .collect::<Vec<_>>()
        })
    });

    let summary = move || {
        let run = running.get();
        items.with(|all| {
            if data.rows.with(Option::is_none) {
                return "tracked Hugging Face files".to_string();
            }
            let repos = all
                .iter()
                .map(|i| i.repo.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            let bytes: u64 = all
                .iter()
                .filter(|i| i.on_disk != Some(false))
                .filter_map(|i| i.size_bytes)
                .sum();
            let n = |id: &str| {
                all.iter()
                    .filter(|i| i.facet(id, run.contains(&i.id)))
                    .count()
            };
            format!(
                "{} files from {} repos · {} on disk · {} downloading · {} failed · {} updates",
                grouped(all.len() as u64),
                grouped(repos as u64),
                human_bin(bytes),
                n("downloading"),
                n("failed"),
                n("updates"),
            )
        })
    };

    let checking = RwSignal::new(false);
    let check_updates = move |_| {
        if checking.get_untracked() {
            return;
        }
        checking.set(true);
        spawn_local(async move {
            let mut found = 0u64;
            let mut errs: Vec<String> = Vec::new();
            for target in TARGETS {
                match crate::api::post::<Value, _>(
                    "/api/op/hf_set",
                    &json!({ "action": "check_updates", "target": target }),
                )
                .await
                {
                    Ok(v) => {
                        found += v
                            .get("updates_available")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        if let Some(es) = v.get("errors").and_then(Value::as_array) {
                            errs.extend(es.iter().filter_map(|e| e.as_str().map(String::from)));
                        }
                    }
                    Err(e) => errs.push(e.to_string()),
                }
            }
            checking.set(false);
            data.load();
            if !errs.is_empty() {
                toasts.err(format!("update check: {}", errs.join("; ")));
            } else if found > 0 {
                toasts.ok(format!(
                    "{found} update(s) available — re-download to fetch"
                ));
            } else {
                toasts.ok("everything is current");
            }
        });
    };

    let ctx = RowCtx {
        toasts,
        data,
        confirm,
        catalog_refresh: Callback::new(move |()| catalog.refresh()),
    };

    let render_group = move |g: Group| {
        let persist = if g.singles {
            "open.downloads.singles".to_string()
        } else {
            format!("open.downloads.repo.{}", g.repo)
        };
        let open = crate::prefs::persisted_bool(&persist, true);
        let count = if g.files.len() == g.total {
            grouped(g.total as u64)
        } else {
            format!("{} of {}", g.files.len(), g.total)
        };
        let meta = if g.singles {
            format!(
                "{} · one file each, its repo named on the row",
                human_bin(g.bytes)
            )
        } else {
            let users = if g.users.is_empty() {
                "unused".to_string()
            } else {
                format!("used by {}", g.users.join(", "))
            };
            format!("{} · {users}", human_bin(g.bytes))
        };
        let label = if g.singles {
            "Single-file repos".to_string()
        } else {
            g.repo.clone()
        };
        let singles = g.singles;
        let files = StoredValue::new(g.files);
        view! {
            <GroupRow
                colspan=COLS
                label=label
                count=Signal::stored(count)
                open=open
                meta=move || view! { <span title=meta.clone()>{meta.clone()}</span> }
            />
            <Show when=move || open.get() || narrowed.get()>
                <For each=move || files.get_value() key=|f| f.clone() let:f>
                    <FileRow f=f ctx=ctx with_repo=singles/>
                </For>
            </Show>
        }
    };

    view! {
        <PageFrame
            title="Downloads"
            sub=summary
            mode=PageMode::Fill
            actions=move || {
                view! {
                    <button class="btn" disabled=move || checking.get() on:click=check_updates>
                        {move || if checking.get() { "Checking…" } else { "Check for updates" }}
                    </button>
                }
            }
            toolbar=move || {
                view! {
                    <FilterBar
                        query=query
                        placeholder="Filter by repo, file, role or model"
                        shown=shown
                        total=total
                        noun="files"
                        facets=FacetSet { items: facets, active: facet }
                    />
                }
            }
        >
            {move || {
                data.error
                    .get()
                    .map(|e| {
                        let stale = data.rows.with(Option::is_some);
                        view! {
                            <div class="notice err row">
                                {if stale { "Refreshing the list failed: " } else { "Loading downloads failed: " }}
                                {e}
                                <button class="btn ghost sm" on:click=move |_| data.load()>
                                    "Retry"
                                </button>
                            </div>
                        }
                    })
            }}
            {move || {
                data.disk
                    .with(|d| {
                        d.iter()
                            .filter_map(|(t, r)| r.as_ref().err().map(|e| format!("{t}: {e}")))
                            .collect::<Vec<_>>()
                    })
                    .into_iter()
                    .map(|e| {
                        view! {
                            <div class="notice warn">
                                "Could not list a models dir, so its files show no size, users or on-disk state — "
                                {e}
                            </div>
                        }
                    })
                    .collect_view()
            }}
            <div class="fill-pane card pad0">
                <table class="data dl-table">
                    <thead>
                        <tr>
                            <th>"File"</th>
                            <th class="col-p2">"Role"</th>
                            <th class="col-p3">"Target"</th>
                            <th class="col-p2 users">"Used by"</th>
                            <th>"Status"</th>
                            <th class="num-h">"Size"</th>
                            <th></th>
                        </tr>
                    </thead>
                    <tbody>
                        <Show when=move || data.rows.with(Option::is_none) && data.error.with(Option::is_none)>
                            <tr>
                                <td colspan=COLS class="dim">"Loading…"</td>
                            </tr>
                        </Show>
                        <Show when=move || data.rows.with(|r| r.as_ref().is_some_and(Vec::is_empty))>
                            <tr>
                                <td colspan=COLS class="empty">
                                    "Nothing tracked yet — files added from Hugging Face (Models → Add from Hugging Face) appear here."
                                </td>
                            </tr>
                        </Show>
                        <For each=move || groups.get() key=|g| g.clone() let:g>
                            {render_group(g)}
                        </For>
                    </tbody>
                </table>
            </div>
            <DeleteModal confirm=confirm ctx=ctx/>
        </PageFrame>
    }
}

/// What a row reaches, captured at render: handlers run without an owner to
/// look context up in.
#[derive(Clone, Copy)]
struct RowCtx {
    toasts: Toasts,
    data: DlData,
    confirm: RwSignal<Option<FileItem>>,
    catalog_refresh: Callback<()>,
}

impl RowCtx {
    fn hf_set(self, action: &'static str, f: &FileItem, ok: &'static str) {
        let body = json!({ "action": action, "id": f.id, "target": f.target });
        spawn_local(async move {
            match crate::api::post::<Value, _>("/api/op/hf_set", &body).await {
                Ok(_) => {
                    self.toasts.ok(ok);
                    self.data.load();
                }
                Err(e) => self.toasts.err(e.to_string()),
            }
        });
    }
}

/// The live job wins when one is running; otherwise the durable status decides,
/// including the derived *interrupted* state — a row that claims to be
/// transferring with no job behind it is one the last shutdown cut off.
fn status_chip(f: &FileItem, job: Option<&JobRow>) -> AnyView {
    if let Some(j) = job {
        let label = j.percent.map(|p| format!("{p}%")).unwrap_or_else(|| {
            // No Content-Length: show what it is doing rather than a fake bar.
            if j.stage.is_empty() {
                "downloading".into()
            } else {
                j.stage.clone()
            }
        });
        return view! {
            <span class="chip live"><span class="dot"></span>{label}</span>
        }
        .into_any();
    }
    if f.in_flight() {
        return view! {
            <span class="chip err" title="No transfer is running for it: the last shutdown cut it off">
                <span class="dot"></span>
                "interrupted"
            </span>
        }
        .into_any();
    }
    match f.status.as_str() {
        "done" if f.on_disk == Some(false) => view! {
            <span class="chip err" title="Tracked as downloaded, but the file is not in the models dir any more">
                <span class="dot"></span>
                "missing"
            </span>
        }
        .into_any(),
        "done" => view! { <span class="chip ok"><span class="dot"></span>"done"</span> }.into_any(),
        "failed" => view! {
            <span class="chip err" title=f.error.clone().unwrap_or_default()>
                <span class="dot"></span>
                "failed"
            </span>
        }
        .into_any(),
        "update_available" => view! {
            <span class="chip live" title="the file changed on Hugging Face">
                <span class="dot"></span>
                "update available"
            </span>
        }
        .into_any(),
        other => {
            view! { <span class="chip off"><span class="dot"></span>{other.to_string()}</span> }
                .into_any()
        }
    }
}

/// `with_repo`: the row stands for its whole repo (a single-file one), so the
/// file name leads with the repo, dimmed.
#[component]
fn FileRow(f: FileItem, ctx: RowCtx, #[prop(optional)] with_repo: bool) -> impl IntoView {
    let id = f.id;
    // This row's transfer, straight off the shared feed.
    let bus = crate::live::use_live();
    let job = Memo::new(move |_| {
        let key = job_key(id);
        bus.jobs
            .get()
            .unwrap_or_default()
            .into_iter()
            .find(|j| j.key.as_deref() == Some(key.as_str()))
    });
    let running = move || job.with(Option::is_some);
    let item = StoredValue::new(f.clone());

    // The one action the row's state asks for, if any; the rest are in "⋯".
    let redl_label = match f.status.as_str() {
        "update_available" => Some("Update"),
        "failed" => Some("Retry"),
        // Nothing is transferring it and the row says it is: cut off mid-flight.
        "queued" | "downloading" => Some("Resume"),
        _ if f.on_disk == Some(false) => Some("Download again"),
        _ => None,
    };

    let planning = RwSignal::new(false);
    let plan_create = move |_| {
        if planning.get_untracked() {
            return;
        }
        planning.set(true);
        let rel = item.with_value(|f| f.rel.clone());
        spawn_local(async move {
            let plan = match crate::api::get::<PlanResult>(format!(
                "/api/local-model-plan?path={}",
                urlenc(&rel)
            ))
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    planning.set(false);
                    ctx.toasts.err(format!("plan failed: {e}"));
                    return;
                }
            };
            let mut args = plan.params.clone();
            args.insert("action".into(), json!("create"));
            args.insert("model_id".into(), json!(plan.model_id));
            args.insert("gguf_path".into(), json!(rel));
            let res =
                crate::api::post::<Value, _>("/api/op/local_model_set", &Value::Object(args)).await;
            planning.set(false);
            match res {
                Ok(_) => {
                    ctx.toasts.ok(format!(
                        "'{}' created from the planned parameters — start it from Models when ready",
                        plan.model_id
                    ));
                    ctx.catalog_refresh.try_run(());
                    ctx.data.load();
                }
                Err(e) => ctx.toasts.err(e.to_string()),
            }
        });
    };

    let primary = move || {
        if running() {
            return Some(
                view! {
                    <button
                        class="btn ghost sm"
                        title="Stop this transfer; the entry stays and can be resumed"
                        on:click=move |_| item.with_value(|f| ctx.hf_set("cancel", f, "cancel requested"))
                    >
                        "Cancel"
                    </button>
                }
                .into_any(),
            );
        }
        if let Some(label) = redl_label {
            return Some(
                view! {
                    <button
                        class="btn sm"
                        on:click=move |_| item.with_value(|f| ctx.hf_set("redownload", f, "download started"))
                    >
                        {label}
                    </button>
                }
                .into_any(),
            );
        }
        item.with_value(|f| f.planable).then(|| {
            view! {
                <button
                    class="btn sm"
                    disabled=move || planning.get()
                    title="Read the GGUF metadata and create a configured model from it"
                    on:click=plan_create
                >
                    {move || if planning.get() { "Planning…" } else { "Plan & create" }}
                </button>
            }
            .into_any()
        })
    };

    let menu = Signal::derive(move || {
        let busy = running();
        let mut v = Vec::new();
        if redl_label.is_none() && item.with_value(|f| f.status == "done") {
            v.push(
                MenuItem::new("Re-download", move || {
                    item.with_value(|f| ctx.hf_set("redownload", f, "download started"))
                })
                .disabled(busy)
                .title("Fetch the file again from Hugging Face"),
            );
        }
        v.push(
            MenuItem::new("Copy path", move || {
                let rel = item.with_value(|f| f.rel.clone());
                let _ = window().navigator().clipboard().write_text(&rel);
            })
            .title("Its path under the models dir, as a model's file field takes it"),
        );
        v.push(
            MenuItem::new("Delete file…", move || {
                ctx.confirm.set(Some(item.get_value()))
            })
            .disabled(busy)
            .title(if busy {
                "Cancel the transfer first"
            } else {
                "Untrack it and delete the file from disk"
            }),
        );
        v
    });

    let chip = move || item.with_value(|f| status_chip(f, job.get().as_ref()));
    let users = match f.used_by.as_slice() {
        [] => None,
        [one] => Some(one.clone()),
        [first, rest @ ..] => Some(format!("{first} +{}", rest.len())),
    };
    let users_title = f.used_by.join(", ");
    let role_title = if f.role_known {
        "what the model that uses it names it as"
    } else {
        "guessed from the filename"
    };
    let name = f.rel.rsplit('/').next().unwrap_or(&f.rel).to_string();
    let sub = f.file.strip_suffix(&name).unwrap_or("").to_string();
    let sub = if with_repo {
        format!("{}/{sub}", f.repo)
    } else {
        sub
    };
    // The users column is the first to go on a narrow table; the name's
    // tooltip keeps them.
    let file_title = if f.used_by.is_empty() {
        format!("{}\nused by no model", f.rel)
    } else {
        format!("{}\nused by {}", f.rel, f.used_by.join(", "))
    };
    let file_title = if with_repo {
        format!("{}\n{file_title}", f.repo)
    } else {
        file_title
    };
    let (rev_short, rev_long) = f.revision.clone();
    let file_title = match rev_long.is_empty() {
        true => file_title,
        false => format!("{file_title}\n{rev_long}"),
    };
    view! {
        <tr class:companion=f.companion>
            <td class="clip mono-sm" title=file_title>
                {(!sub.is_empty()).then(|| view! { <span class="pfx">{sub.clone()}</span> })}
                {name}
                {(!rev_short.is_empty()).then(|| view! { <span class="dim">" " {rev_short}</span> })}
            </td>
            <td class="col-p2">
                <span class="type-badge" title=role_title>{f.role.clone()}</span>
            </td>
            <td class="col-p3 dim">{f.target.clone()}</td>
            <td class="col-p2 dim users" title=users_title>
                {users.unwrap_or_else(|| "unused".to_string())}
            </td>
            <td>{chip}</td>
            <td class="num">{f.size.clone()}</td>
            <td class="actions">{primary} <RowMenu items=menu/></td>
        </tr>
    }
}

/// The one delete question for the page: untracking removes the file from
/// disk, and a file a model still uses leaves that model unable to start.
#[component]
fn DeleteModal(confirm: RwSignal<Option<FileItem>>, ctx: RowCtx) -> impl IntoView {
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = confirm.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && confirm.with_untracked(Option::is_some) {
            confirm.set(None);
        }
    });
    view! {
        <Modal open=open title="Delete a downloaded file">
            {move || {
                confirm
                    .get()
                    .map(|f| {
                        let item = StoredValue::new(f.clone());
                        view! {
                            <p>
                                "Untrack " <span class="mono-sm">{f.rel.clone()}</span>
                                " and delete the file from disk?"
                            </p>
                            {(!f.used_by.is_empty())
                                .then(|| {
                                    view! {
                                        <div class="notice warn">
                                            <b>{format!("Used by {}", f.used_by.join(", "))}</b>
                                            " — it will not start until the file is back."
                                        </div>
                                    }
                                })}
                            <ModalFooter>
                                <button class="btn ghost" on:click=move |_| open.set(false)>
                                    "Keep it"
                                </button>
                                <button
                                    class="btn danger"
                                    on:click=move |_| {
                                        open.set(false);
                                        item.with_value(|f| {
                                            ctx.hf_set("delete", f, "untracked and removed from disk")
                                        });
                                    }
                                >
                                    "Delete file"
                                </button>
                            </ModalFooter>
                        }
                    })
            }}
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    use super::{human_bin, name_guess, parse_size};

    #[test]
    fn sizes_round_trip_through_their_label() {
        assert_eq!(parse_size("644.2 MiB"), Some(675_492_659));
        assert_eq!(
            parse_size("15.9 GiB").map(human_bin).as_deref(),
            Some("15.9 GiB")
        );
        assert_eq!(parse_size(""), None);
        assert_eq!(human_bin(512), "512 B");
    }

    #[test]
    fn a_missing_file_is_guessed_like_the_server_does() {
        assert_eq!(name_guess("r/mtp-gemma-4-26B-A4B-it.gguf"), "drafter");
        assert_eq!(name_guess("r/mmproj-BF16.gguf"), "mmproj");
        assert_eq!(name_guess("r/model-Q4_K_M.gguf"), "weights");
        assert_eq!(
            name_guess("r/Qwen3.6-27B-NEO-MAX-NEO-MTP-IQ4_XS.gguf"),
            "weights"
        );
        assert_eq!(name_guess("r/vae/ae.safetensors"), "other");
    }
}
