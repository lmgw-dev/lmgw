//! The Builds tab: one row per build, its run history unfolded under it, and
//! the two row modals (Check merge, Delete).

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::builds::{
    BuildCheckMergeArgs, BuildRun, BuildRunStatus, BuildSetAction, BuildSetArgs, BuildSetResponse,
    BuildView, CheckMergeReport, ImageUseKind, KeptImage, UpdateStatus,
};

use super::editor::EditorSeed;
use super::reports::MergeReportView;
use super::{
    ago, engine_chip, extra_short, local_ts, run_duration, run_tag, sha7, short_repo, status_chip,
    tag_only, use_bk, use_chips, Bk, LogTarget,
};
use crate::backends_api as api;
use crate::fmt::human_bytes;
use crate::scope::Scope;
use crate::widgets::{ConfirmButton, CopyBtn, MenuItem, Modal, ModalFooter, RowMenu};

/// The builds table's column count (the actions column included).
const COLS: u32 = 9;
/// Runs per history page. Said on the page ("the newest 10") and paged
/// further back with `before`, never a silent cap.
const PAGE: u32 = 10;

#[component]
pub fn BuildsTab() -> impl IntoView {
    let bk = use_bk();
    let loaded = move || bk.builds.with(Option::is_some);
    let empty = move || bk.builds.with(|b| b.as_ref().is_some_and(Vec::is_empty));
    let build_ids = Memo::new(move |_| {
        bk.builds.with(|b| {
            b.as_ref()
                .map(|b| b.iter().map(|v| v.build.id).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    view! {
        {move || {
            bk.builds_err
                .get()
                .map(|e| {
                    let stale = bk.builds.with(Option::is_some);
                    view! {
                        <div class="notice err row">
                            {if stale { "Refreshing the builds failed: " } else { "Loading the builds failed: " }}
                            {e}
                            <button class="btn ghost sm" on:click=move |_| bk.load_builds()>
                                "Retry"
                            </button>
                        </div>
                    }
                })
        }}
        {move || {
            bk.usage_err
                .get()
                .map(|e| {
                    view! {
                        <div class="notice warn">
                            <b>"Who uses which image could not be told"</b>
                            {format!(
                                "{e}. Every \u{201c}used by\u{201d} on this page is empty for that reason, not because nothing uses the image."
                            )}
                        </div>
                    }
                })
        }}
        <div class="fill-pane card pad0">
            <Show
                when=empty
                fallback=move || {
                    view! {
                        <table class="data bk-builds">
                            <thead>
                                <tr>
                                    <th>"Engine"</th>
                                    <th>"Build"</th>
                                    <th class="bk-early">"Repository"</th>
                                    <th>"Ref"</th>
                                    <th class="col-p2">"Extras"</th>
                                    <th>"Last run"</th>
                                    <th class="bk-mid">"Image"</th>
                                    <th class="col-p3 num-h" title="Class defaults, model overrides and containers (running or stopped) on its moving tag">
                                        "Used by"
                                    </th>
                                    <th></th>
                                </tr>
                            </thead>
                            <tbody>
                                <Show when=move || !loaded() && bk.builds_err.with(Option::is_none)>
                                    <tr>
                                        <td colspan=COLS class="dim">"Loading…"</td>
                                    </tr>
                                </Show>
                                // Keyed by the build: a re-read list updates
                                // the row in place, so an open menu, an armed
                                // confirm and an unfolded history survive it.
                                <For each=move || build_ids.get() key=|id| *id let:id>
                                    <BuildRow id=id/>
                                </For>
                            </tbody>
                        </table>
                    }
                }
            >
                <EmptyState/>
            </Show>
        </div>
    }
}

/// No builds yet: what a build is, and the four repositories to start from.
#[component]
fn EmptyState() -> impl IntoView {
    let bk = use_bk();
    bk.ensure_env();
    let presets = move || match bk.env.get() {
        Some(Ok(env)) => env.repo_presets,
        _ => Vec::new(),
    };
    view! {
        <div class="bk-empty">
            <p>
                "No builds yet. A build turns a git repository — upstream or a fork, with pull "
                "requests merged on top — into a container image for one of lmgw's engines. "
                "Each run is checked on the GPU before anything uses it, and every image keeps "
                "a record of exactly what went into it."
            </p>
            <div class="row">
                <For each=presets key=|p| p.id.clone() let:p>
                    {
                        let label = p.name.clone();
                        let title = format!("{} · {}", super::engine_label(p.engine), p.repo_url);
                        let seed = StoredValue::new(EditorSeed::from_preset(&p));
                        view! {
                            <button
                                class="btn"
                                title=title
                                on:click=move |_| bk.editor.set(Some(seed.get_value()))
                            >
                                {label}
                            </button>
                        }
                    }
                </For>
                <button
                    class="btn ghost"
                    on:click=move |_| bk.editor.set(Some(EditorSeed::blank()))
                >
                    "Custom URL…"
                </button>
            </div>
            {move || match bk.env.get() {
                None => Some(view! { <div class="dim">"Loading the repository presets…"</div> }.into_any()),
                Some(Err(e)) => {
                    Some(
                        view! {
                            <div class="notice err row">
                                "The build environment did not load: "
                                {e}
                                <button class="btn ghost sm" on:click=move |_| bk.retry_env()>
                                    "Retry"
                                </button>
                            </div>
                        }
                            .into_any(),
                    )
                }
                Some(Ok(_)) => None,
            }}
        </div>
    }
}

/// What a build is called on the page: its name, else its slug.
fn display_name(v: &BuildView) -> String {
    if v.build.spec.name.is_empty() {
        v.build.spec.slug.clone()
    } else {
        v.build.spec.name.clone()
    }
}

/// The builds table's row for build `id`. What it shows follows the list as
/// it is re-read; the row itself stays until the build leaves the list.
#[component]
fn BuildRow(id: i64) -> impl IntoView {
    let bk = use_bk();
    // A row whose build just left the list keeps its last view until the
    // `For` drops it.
    let v = Memo::new(move |prev: Option<&BuildView>| {
        bk.builds
            .with(|b| {
                b.as_ref()
                    .and_then(|b| b.iter().find(|x| x.build.id == id).cloned())
            })
            .or_else(|| prev.cloned())
            .unwrap_or_default()
    });
    let name = Memo::new(move |_| v.with(display_name));
    let moving = Memo::new(move |_| v.with(|v| v.moving_tag.clone()));
    let live = Memo::new(move |_| bk.live.with(|m| m.get(&id).cloned()));
    // The job alone, for what must not be redrawn on every progress tick (a
    // button under the pointer, the menu).
    let live_job = Memo::new(move |_| live.with(|l| l.as_ref().map(|l| l.job_id)));
    let is_live = move || live_job.with(Option::is_some);
    let open = move || bk.expanded.with(|s| s.contains(&id));
    let toggle = move |_| {
        bk.expanded.update(|s| {
            if !s.remove(&id) {
                s.insert(id);
            }
        })
    };

    let status_cell = move || {
        let l = live.get();
        let last_status = v.with(|v| v.last_run.as_ref().map(|r| r.status));
        match (l.as_ref(), last_status) {
            (Some(l), _) => status_chip(BuildRunStatus::Running, Some(l)),
            (None, Some(s)) => status_chip(s, None),
            (None, None) => view! { <span class="dim">"never run"</span> }.into_any(),
        }
    };
    let when = move || {
        v.with(|v| {
            v.last_run.as_ref().map(|r| {
                r.finished_at
                    .clone()
                    .unwrap_or_else(|| r.started_at.clone())
            })
        })
        .map(|w| {
            let title = local_ts(&w);
            view! { <span class="dim bk-when" title=title>{ago(&w)}</span> }
        })
    };

    let current_tag = Memo::new(move |_| v.with(|v| v.current_run.as_ref().and_then(run_tag)));
    let image_title = move || match current_tag.get() {
        Some(t) => format!("{}\n→ {t}", moving.get()),
        None => format!("{}\nnot built yet", moving.get()),
    };
    let image_cell = move || {
        current_tag
            .get()
            .as_deref()
            .map(|t| tag_only(t).to_string())
            .unwrap_or_else(|| "—".to_string())
    };

    let users = move || v.with(|v| v.used_by.len());
    let users_title = move || {
        v.with(|v| {
            if v.used_by.is_empty() {
                "nothing uses its moving tag".to_string()
            } else {
                v.used_by
                    .iter()
                    .map(super::use_label)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        })
    };

    let extras_title = move || {
        v.with(|v| {
            v.build
                .spec
                .extras
                .iter()
                .map(|e| {
                    let pin = e
                        .pin()
                        .map(|p| format!(" @ {}", sha7(p)))
                        .unwrap_or_default();
                    format!("{}{pin}", e.label())
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
    };
    let extras_chips = move || {
        let extras = v.with(|v| v.build.spec.extras.clone());
        if extras.is_empty() {
            return view! { <span class="dim">"—"</span> }.into_any();
        }
        extras
            .iter()
            .map(|e| {
                let pin = e.pin().map(|p| format!(" @{}", sha7(p)));
                let pinned = pin.is_some();
                view! {
                    <span class="type-badge" class:bk-pinned=pinned>
                        {extra_short(e)}
                        {pin}
                    </span>
                }
            })
            .collect_view()
            .into_any()
    };

    let primary = move || {
        if is_live() {
            view! {
                <button
                    class="btn ghost sm"
                    title="The live log, the phase and Cancel"
                    on:click=move |_| v.with_untracked(|v| bk.open_log(v))
                >
                    "Log"
                </button>
            }
            .into_any()
        } else {
            view! {
                <button
                    class="btn sm"
                    title="Fetch, merge, build, verify and tag — skipped when these exact inputs already have a verified image"
                    on:click=move |_| bk.run(id, name.get_untracked(), false)
                >
                    "Run"
                </button>
            }
            .into_any()
        }
    };

    let menu = Signal::derive(move || {
        let live_job = live_job.get();
        let has_run = v.with(|v| v.last_run.is_some());
        let mut items = vec![
            MenuItem::new("Rebuild anyway", move || bk.run(id, name.get_untracked(), true))
                .disabled(live_job.is_some())
                .title(
                    "Build even when these exact inputs already have a verified image, pulling newer base images",
                ),
            MenuItem::new(
                if live_job.is_some() { "View live log" } else { "View last log" },
                move || v.with_untracked(|v| bk.open_log(v)),
            )
            .disabled(!has_run && live_job.is_none()),
            MenuItem::new("Edit…", move || {
                bk.ensure_env();
                bk.editor.set(Some(v.with_untracked(EditorSeed::edit)));
            }),
            MenuItem::new("Duplicate", move || duplicate(bk, id, name.get_untracked()))
                .title("A copy to change: the quick way to a variant with one more PR"),
            MenuItem::new("Check merge", move || {
                bk.merge.set(Some((id, name.get_untracked())))
            })
            .title("Fetch and merge every extra onto the base in a throwaway worktree — nothing is built"),
            MenuItem::new("Check for updates", move || {
                bk.check_updates(Some(id), name.get_untracked())
            })
            .disabled(bk.checking.with(|c| c.contains(&id) || c.contains(&0)))
            .title("Ask the remote now whether the ref or an unpinned extra moved since the last verified run"),
        ];
        if let Some(job) = live_job {
            items.push(
                MenuItem::new("Cancel run", move || bk.cancel(job, name.get_untracked()))
                    .danger()
                    .title("Stop the build; the compile cache keeps what it compiled"),
            );
        }
        items.push(
            MenuItem::new("Delete…", move || {
                bk.deleting.set(Some(v.get_untracked()))
            })
            .disabled(live_job.is_some())
            .title(if live_job.is_some() {
                "Cancel the run first"
            } else {
                "Delete the build; its runs stay in the history"
            }),
        );
        items
    });

    let spec = move |f: fn(&lmgw_api_types::builds::BuildSpec) -> String| {
        move || v.with(|v| f(&v.build.spec))
    };
    let repo_title = spec(|s| s.repo_url.clone());
    let repo_short = spec(|s| short_repo(&s.repo_url));
    let git_ref = spec(|s| s.git_ref.clone());
    let slug = spec(|s| s.slug.clone());
    let name_title = move || {
        format!(
            "{}\n{}\n{}",
            name.get(),
            v.with(|v| v.build.spec.repo_url.clone()),
            moving.get()
        )
    };
    let flags = move || v.with(|v| v.update.as_ref().map(update_flags));
    view! {
        <tr class:bk-open=open>
            <td>{move || engine_chip(v.with(|v| v.build.spec.engine))}</td>
            <td class="clip" title=name_title>
                <button
                    type="button"
                    class="link-btn bk-toggle"
                    aria-expanded=move || open().to_string()
                    title="Run history"
                    on:click=toggle
                >
                    <span class="caret-icon" aria-hidden="true">"▸"</span>
                    {move || name.get()}
                </button>
                <span class="pfx mono-sm bk-slug">{slug}</span>
                {flags}
            </td>
            <td class="bk-early mono-sm bk-cut-s" title=repo_title>{repo_short}</td>
            <td class="mono-sm bk-cut-s" title=git_ref>{git_ref}</td>
            <td class="col-p2 bk-cut-s" title=extras_title>
                <span class="bk-chips">{extras_chips}</span>
            </td>
            <td>
                <span class="bk-status">{status_cell} {when}</span>
            </td>
            <td class="bk-mid mono-sm bk-cut-s" title=image_title>{image_cell}</td>
            <td class="col-p3 num" title=users_title>{users}</td>
            <td class="actions">
                <span class="row-acts">{primary} <RowMenu items=menu/></span>
            </td>
        </tr>
        <Show when=open>
            <tr class="detail-row">
                <td colspan=COLS>
                    <RunHistory v=v name=name moving=moving/>
                </td>
            </tr>
        </Show>
    }
}

/// The full story of an update status, for a tooltip: the reasons, what
/// each unpinned extra did, what could not be checked, and when.
pub fn update_tooltip(u: &UpdateStatus) -> String {
    let mut lines: Vec<String> = u.reasons.clone();
    for x in &u.extras {
        let line = format!("{}: {}", x.label, x.change);
        if !lines
            .iter()
            .any(|l| l.contains(&x.label) && l.contains(&x.change))
        {
            lines.push(line);
        }
    }
    lines.extend(u.errors.iter().cloned());
    if lines.is_empty() {
        lines.push("up to date".to_string());
    }
    if !u.checked_at.is_empty() {
        lines.push(format!(
            "checked {} ({})",
            ago(&u.checked_at),
            local_ts(&u.checked_at)
        ));
    }
    lines.join("\n")
}

/// A build row's update chips (§8): the first reason itself ("master +37
/// commits"), with how many more; and a separate chip when something could
/// not be checked — a rate-limited forge is not an update. Nothing when up
/// to date.
fn update_flags(u: &UpdateStatus) -> impl IntoView {
    let title = update_tooltip(u);
    let reason = u.reasons.first().cloned().map(|first| {
        let more = u.reasons.len() - 1;
        view! {
            <span class="chip warn bk-flag" title=title.clone()>
                <span class="dot"></span>
                <span class="bk-flag-t">{first}</span>
                {(more > 0).then(|| view! { <span class="bk-flag-n">{format!("+{more}")}</span> })}
            </span>
        }
    });
    let failed = (!u.errors.is_empty()).then(|| {
        view! {
            <span class="chip off bk-flag" title=title.clone()>
                <span class="dot"></span>
                "check failed"
            </span>
        }
    });
    view! { {reason} {failed} }
}

fn duplicate(bk: Bk, id: i64, name: String) {
    let args = BuildSetArgs {
        action: BuildSetAction::Duplicate,
        id: Some(id),
        ..Default::default()
    };
    spawn_local(async move {
        match api::build_set(&args).await {
            Ok(r) => {
                bk.load_builds();
                match r.build {
                    Some(b) => {
                        bk.toasts.ok(format!(
                            "{name}: duplicated as '{}' — rename it here",
                            b.spec.slug
                        ));
                        if bk.scope.alive() {
                            bk.ensure_env();
                            bk.editor.set(Some(EditorSeed::edit_build(b, false, None)));
                        }
                    }
                    None => bk.toasts.ok(format!("{name}: duplicated")),
                }
            }
            Err(e) => bk.toasts.err(format!("{name}: {e}")),
        }
    });
}

/// A list read asked for again while one is out (review LOW): the answer in
/// flight may predate what changed, so one more read follows it — one,
/// however often it was asked meanwhile.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReadQueue {
    out: bool,
    again: bool,
}

impl ReadQueue {
    /// Ask for a read: `true` when it goes out now; otherwise one follows the
    /// read in flight.
    pub fn ask(&mut self) -> bool {
        if self.out {
            self.again = true;
            false
        } else {
            self.out = true;
            true
        }
    }

    /// The read in flight answered: `true` when a re-read was asked for
    /// meanwhile and should be made now.
    pub fn done(&mut self) -> bool {
        self.out = false;
        std::mem::take(&mut self.again)
    }

    pub fn out(&self) -> bool {
        self.out
    }
}

/// A build's runs, newest first, a page at a time. The facts above them
/// follow the build as the list is re-read.
#[component]
fn RunHistory(v: Memo<BuildView>, name: Memo<String>, moving: Memo<String>) -> impl IntoView {
    let bk = use_bk();
    let id = v.with_untracked(|v| v.build.id);
    let runs = RwSignal::new(None::<Vec<BuildRun>>);
    let more = RwSignal::new(false);
    let err = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let queue = StoredValue::new(ReadQueue::default());
    // Bumped when a re-read asked for while a read was out is due.
    let reread = RwSignal::new(0u64);
    let scope = Scope::new();

    let load = move |older: bool| {
        let starts = if older {
            // Paging back waits for nothing: its button is off while a read
            // is out.
            if queue.with_value(ReadQueue::out) {
                return;
            }
            queue.update_value(|q| {
                q.ask();
            });
            true
        } else {
            queue.try_update_value(ReadQueue::ask).unwrap_or(false)
        };
        if !starts {
            return;
        }
        let shown = runs.with_untracked(|r| r.as_ref().map_or(0, Vec::len));
        let (limit, before) = if older {
            (
                PAGE,
                runs.with_untracked(|r| r.as_ref().and_then(|r| r.last().map(|x| x.id))),
            )
        } else {
            // A re-read keeps the pages already unfolded.
            ((shown as u32).max(PAGE), None)
        };
        loading.set(true);
        scope.spawn(async move {
            let res = api::build_get(id, limit, before).await;
            loading.set(false);
            match res {
                Ok(r) => {
                    err.set(None);
                    more.set(r.more);
                    if older {
                        runs.update(|v| v.get_or_insert_with(Vec::new).extend(r.runs));
                    } else {
                        runs.set(Some(r.runs));
                    }
                }
                Err(e) => err.set(Some(e.to_string())),
            }
            if queue.try_update_value(ReadQueue::done) == Some(true) {
                reread.update(|n| *n += 1);
            }
        });
    };
    // The first page, and again whenever the builds list is re-read: a run
    // started, ended, was verified or made current.
    Effect::new(move |_| {
        bk.rev.track();
        reread.track();
        load(false);
    });

    let ctx = RunCtx {
        bk,
        build_id: id,
        name,
        current_image: Memo::new(move |_| {
            v.with(|v| v.current_run.as_ref().and_then(|r| r.image_id.clone()))
        }),
    };
    let notes = move || v.with(|v| v.build.spec.notes.clone());
    let uses = move || v.with(|v| v.used_by.clone());
    let has_verified = move || {
        v.with(|v| {
            v.current_run.is_some()
                || v.last_run.as_ref().is_some_and(|r| {
                    matches!(
                        r.status,
                        BuildRunStatus::Succeeded | BuildRunStatus::Unverified
                    )
                })
        })
    };
    let checking = move || bk.checking.with(|c| c.contains(&id) || c.contains(&0));
    let check_btn = move || {
        view! {
            <button
                class="btn ghost sm"
                disabled=checking
                title="Ask the remote now whether the ref or an unpinned extra moved since the last verified run"
                on:click=move |_| bk.check_updates(Some(id), name.get_untracked())
            >
                {move || if checking() { "Checking…" } else { "Check now" }}
            </button>
        }
    };
    view! {
        <div class="bk-detail">
            <div class="bk-facts">
                {move || {
                    let m = moving.get();
                    view! {
                        <span class="copy-line" title="Picking this tag in a class or model image field follows the build">
                            <span class="dim">"moving tag"</span>
                            <code class="mono-sm">{m.clone()}</code>
                            <CopyBtn text=m/>
                        </span>
                    }
                }}
                {move || {
                    let uses = uses();
                    (!uses.is_empty())
                        .then(|| {
                            view! {
                                <span class="bk-chips">
                                    <span class="dim">"used by"</span>
                                    {use_chips(&uses)}
                                </span>
                            }
                        })
                }}
            </div>
            {move || match v.with(|v| v.update.clone()) {
                Some(u) if u.has_update() => {
                    let checked = (!u.checked_at.is_empty())
                        .then(|| format!("checked {}", ago(&u.checked_at)));
                    let checked_title = local_ts(&u.checked_at);
                    view! {
                        <div class="notice warn bk-upd">
                            <div class="row">
                                <b class="spacer">"Newer inputs than the last verified run"</b>
                                <span class="dim" title=checked_title>{checked}</span>
                                {check_btn()}
                            </div>
                            <ul class="bk-upd-list">
                                {u.reasons.iter().map(|r| view! { <li>{r.clone()}</li> }).collect_view()}
                            </ul>
                            {(!u.extras.is_empty())
                                .then(|| {
                                    view! {
                                        <div class="bk-chips">
                                            <span class="dim">"extras"</span>
                                            {u
                                                .extras
                                                .iter()
                                                .map(|x| {
                                                    view! {
                                                        <span class="type-badge" title=format!("{}: {}", x.label, x.change)>
                                                            {format!("{} {}", x.label, x.change)}
                                                        </span>
                                                    }
                                                })
                                                .collect_view()}
                                        </div>
                                    }
                                })}
                            {u
                                .errors
                                .iter()
                                .map(|e| view! { <div class="status-err mini-note">{e.clone()}</div> })
                                .collect_view()}
                        </div>
                    }
                        .into_any()
                }
                Some(u) => {
                    let checked_title = local_ts(&u.checked_at);
                    let head = if u.errors.is_empty() {
                        format!("Up to date as of {}", ago(&u.checked_at))
                    } else {
                        "The last update check could not finish".to_string()
                    };
                    view! {
                        <div class="bk-upd-line">
                            <span class=if u.errors.is_empty() { "dim" } else { "status-warn" } title=checked_title>
                                {head}
                            </span>
                            {u
                                .errors
                                .iter()
                                .map(|e| view! { <span class="dim">{e.clone()}</span> })
                                .collect_view()}
                            {check_btn()}
                        </div>
                    }
                        .into_any()
                }
                None => {
                    let has_verified = has_verified();
                    view! {
                        <div class="bk-upd-line">
                            <span class="dim">
                                {if has_verified {
                                    "Not checked for updates yet."
                                } else {
                                    "Update checks start once a run has built an image to compare with."
                                }}
                            </span>
                            {has_verified.then(check_btn)}
                        </div>
                    }
                        .into_any()
                }
            }}
            {move || {
                let notes = notes();
                (!notes.trim().is_empty()).then(|| view! { <div class="dim bk-notes">{notes}</div> })
            }}
            {move || {
                err.get()
                    .map(|e| {
                        view! {
                            <div class="notice err row">
                                "The run history did not load: "
                                {e}
                                <button class="btn ghost sm" on:click=move |_| load(false)>
                                    "Retry"
                                </button>
                            </div>
                        }
                    })
            }}
            {move || match runs.get() {
                None if err.with(Option::is_none) => {
                    view! { <div class="dim">"Loading the runs…"</div> }.into_any()
                }
                None => ().into_any(),
                Some(list) if list.is_empty() => {
                    view! { <div class="dim">"No runs yet — Run builds it."</div> }.into_any()
                }
                Some(list) => {
                    view! {
                        <table class="data bk-sub bk-runs">
                            <thead>
                                <tr>
                                    <th>"Status"</th>
                                    <th>"Started"</th>
                                    <th class="num-h">"Took"</th>
                                    <th>"Base"</th>
                                    <th class="col-p2">"Extras"</th>
                                    <th class="num-h">"Size"</th>
                                    <th>"Tag"</th>
                                    <th></th>
                                </tr>
                            </thead>
                            <tbody>
                                {list
                                    .into_iter()
                                    .map(|r| view! { <RunRow run=r ctx=ctx/> })
                                    .collect_view()}
                            </tbody>
                        </table>
                    }
                        .into_any()
                }
            }}
            <Show when=move || more.get()>
                <div class="show-more">
                    <span>
                        {move || {
                            format!(
                                "The newest {} runs",
                                runs.with(|r| r.as_ref().map_or(0, Vec::len)),
                            )
                        }}
                    </span>
                    <button
                        type="button"
                        class="link-btn"
                        disabled=move || loading.get()
                        on:click=move |_| load(true)
                    >
                        {format!("Show {PAGE} older")}
                    </button>
                </div>
            </Show>
        </div>
    }
}

/// What a run row reaches. Owned by the run history: read it before an
/// `.await`, never after — the history can be folded meanwhile.
#[derive(Clone, Copy)]
struct RunCtx {
    bk: Bk,
    build_id: i64,
    name: Memo<String>,
    /// The image the moving tag names now (the promoted run's).
    current_image: Memo<Option<String>>,
}

#[component]
fn RunRow(run: BuildRun, ctx: RunCtx) -> impl IntoView {
    let bk = ctx.bk;
    let run_id = run.id;
    let status = run.status;
    let live = Memo::new(move |_| {
        bk.live.with(|m| {
            m.get(&ctx.build_id)
                .filter(|l| l.detail.run_id == run_id)
                .cloned()
        })
    });
    let tag = run_tag(&run);
    let rebuild = tag.as_deref().and_then(super::rebuild_tag_run).is_some();
    let tag_title = run.tags.join("\n");
    let extras = run
        .inputs
        .resolved
        .as_ref()
        .map(|r| {
            r.extras
                .iter()
                .map(|x| format!("{} {}", extra_short(&x.extra), sha7(&x.sha)))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| run.inputs.config.extras.iter().map(extra_short).collect());
    let extras_line = if extras.is_empty() {
        "—".to_string()
    } else {
        extras.join(", ")
    };
    let base = run.base_sha.clone().unwrap_or_default();
    let image_id = run.image_id.clone();
    let has_image = image_id.is_some();
    let promoted = run.promoted;
    // The moving tag names this run's image: the promoted run's, and every
    // up-to-date run's that reused it (it built nothing of its own). Only the promoted one wears the
    // badge (a column of "current" said nothing); none can be made current.
    let on_current = promoted
        || (run.image_id.is_some() && ctx.current_image.with_untracked(|c| *c == run.image_id));
    let verified = matches!(status, BuildRunStatus::Succeeded | BuildRunStatus::UpToDate);
    let run_sv = StoredValue::new(run.clone());
    let started_title = local_ts(&run.started_at);
    let error_title = run.error.clone().unwrap_or_default();

    let open_log = move || {
        bk.log.set(Some(LogTarget {
            run_id,
            build_id: Some(ctx.build_id),
            name: ctx.name.get_untracked(),
            promoted: None,
        }))
    };
    let menu = Signal::derive(move || {
        let image_id = run_sv.with_value(|r| r.image_id.clone());
        let tags = run_sv.with_value(|r| r.tags.clone());
        let mut items = vec![
            MenuItem::new("Make current", move || promote(ctx, run_id))
                .disabled(!verified || on_current || !has_image)
                .title(if on_current {
                    "Already current: the moving tag points at this image"
                } else if !verified {
                    "Only a verified run can become current"
                } else {
                    "Move the build's moving tag to this run's image (a rollback, for an older run)"
                }),
            MenuItem::new("Verify now", move || verify(ctx, run_id))
                .disabled(!has_image || !status.built())
                .title("Run the --help and GPU device checks on this image again"),
        ];
        if let Some(img) = image_id.clone() {
            items.push(
                MenuItem::new("Retag…", move || {
                    bk.retag.set(Some((img.clone(), tags.clone())))
                })
                .title("Add or remove a tag on this image"),
            );
        }
        if let Some(img) = image_id {
            items.push(
                MenuItem::new("Delete image…", move || {
                    bk.image_delete.set(Some(img.clone()))
                })
                .danger()
                .title(if promoted {
                    "It holds the moving tag — shows what uses it before anything is deleted"
                } else {
                    "Remove this run's image; the run stays in the history"
                }),
            );
        }
        items
    });
    view! {
        <tr>
            <td title=error_title>
                {move || status_chip(status, live.get().as_ref())}
                {promoted.then(|| view! { <span class="type-badge bk-current" title="The moving tag points at this image">"current"</span> })}
            </td>
            <td class="dim" title=started_title>{ago(&run.started_at)}</td>
            <td class="num">{run_duration(&run).unwrap_or_default()}</td>
            <td class="mono-sm" title=base.clone()>{sha7(&base)}</td>
            <td class="col-p2 mono-sm bk-cut" title=extras_line.clone()>{extras_line.clone()}</td>
            <td class="num">{run.size_bytes.map(human_bytes).unwrap_or_default()}</td>
            <td class="mono-sm bk-cut" title=tag_title>
                {rebuild
                    .then(|| {
                        view! {
                            <span
                                class="type-badge"
                                title="Not verified, so it did not take the immutable tag: a model pinned to that tag keeps the verified image"
                            >
                                "unverified rebuild"
                            </span>
                        }
                    })}
                {tag.as_deref().map(|t| tag_only(t).to_string()).unwrap_or_else(|| "—".to_string())}
            </td>
            <td class="actions">
                <span class="row-acts">
                    <button class="btn ghost sm" on:click=move |_| open_log()>
                        "Log"
                    </button>
                    <RowMenu items=menu/>
                </span>
            </td>
        </tr>
    }
}

fn promote(ctx: RunCtx, run_id: i64) {
    let bk = ctx.bk;
    let (build_id, name) = (ctx.build_id, ctx.name.get_untracked());
    spawn_local(async move {
        match api::build_promote(run_id).await {
            Ok(p) => {
                bk.toasts.ok(format!(
                    "{} now points at {}",
                    tag_only(&p.moving_tag),
                    sha7(&p.to_image)
                ));
                bk.load_builds();
                if bk.scope.alive() {
                    // The same recreate offer a finished run gets (§6).
                    bk.log.set(Some(LogTarget {
                        run_id,
                        build_id: Some(build_id),
                        name,
                        promoted: Some(p),
                    }));
                }
            }
            Err(e) => bk.toasts.err(format!("make current: {e}")),
        }
    });
}

fn verify(ctx: RunCtx, run_id: i64) {
    let bk = ctx.bk;
    spawn_local(async move {
        match api::build_verify(run_id).await {
            Ok(r) => {
                if r.gpu_verified {
                    bk.toasts.ok(format!(
                        "GPU-verified: {}",
                        if r.devices.is_empty() {
                            "device found".to_string()
                        } else {
                            r.devices.join(", ")
                        }
                    ));
                } else {
                    let why = if r.notes.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", r.notes.join("; "))
                    };
                    bk.toasts.warn(format!("not GPU-verified{why}"));
                }
                bk.load_builds();
            }
            Err(e) => bk.toasts.err(format!("verify: {e}")),
        }
    });
}

/// Delete an image nothing uses (the Images tab's inline confirm). Refused
/// when something started using it meanwhile: the list is re-read, so the
/// next Delete opens the confirmation that names the users.
pub fn delete_image(bk: Bk, image: String) {
    spawn_local(async move {
        match api::container_image_delete(image.clone(), false).await {
            Ok(r) => {
                bk.images_notice.set(None);
                bk.toasts.ok(if r.removed.is_empty() {
                    "nothing was removed".to_string()
                } else {
                    format!("removed {}", r.removed.join(", "))
                });
                bk.load_builds();
                bk.load_images();
            }
            Err(e) => {
                let msg = format!("{} was not deleted: {e}", sha7(&image));
                bk.images_notice.set(Some(msg.clone()));
                bk.toasts.err(msg);
                bk.load_images();
            }
        }
    });
}

/// "Check merge" from a row: the saved build's extras merged in a throwaway
/// worktree.
#[component]
pub fn MergeModal() -> impl IntoView {
    let bk = use_bk();
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = bk.merge.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && bk.merge.with_untracked(Option::is_some) {
            bk.merge.set(None);
        }
    });
    view! {
        <Modal open=open title="Check merge">
            {move || bk.merge.get().map(|(id, name)| view! { <MergeBody id=id name=name/> })}
        </Modal>
    }
}

#[component]
fn MergeBody(id: i64, name: String) -> impl IntoView {
    let report = RwSignal::new(None::<Result<CheckMergeReport, String>>);
    let scope = Scope::new();
    let run = move || {
        report.set(None);
        scope.spawn(async move {
            let args = BuildCheckMergeArgs {
                id: Some(id),
                spec: None,
            };
            let res = api::build_check_merge(&args)
                .await
                .map_err(|e| e.to_string());
            report.set(Some(res));
        });
    };
    run();
    view! {
        <p class="dim">
            "Fetches the base and every extra of " <b>{name}</b>
            ", then merges them in order in a throwaway worktree — the same code a run uses. "
            "Nothing is built."
        </p>
        {move || match report.get() {
            None => view! { <div class="dim">"Fetching and merging…"</div> }.into_any(),
            Some(Err(e)) => view! { <div class="wiz-err">{e}</div> }.into_any(),
            Some(Ok(r)) => view! { <MergeReportView r=r/> }.into_any(),
        }}
        <ModalFooter>
            <button
                class="btn ghost"
                disabled=move || report.with(Option::is_none)
                on:click=move |_| run()
            >
                "Check again"
            </button>
        </ModalFooter>
    }
}

/// Deleting a build: its runs stay (detached), its images only on request.
#[component]
pub fn DeleteBuildModal() -> impl IntoView {
    let bk = use_bk();
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = bk.deleting.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && bk.deleting.with_untracked(Option::is_some) {
            bk.deleting.set(None);
        }
    });
    view! {
        <Modal open=open title="Delete build">
            {move || bk.deleting.get().map(|v| view! { <DeleteBody v=v open=open/> })}
        </Modal>
    }
}

#[component]
fn DeleteBody(v: BuildView, open: RwSignal<bool>) -> impl IntoView {
    let bk = use_bk();
    let with_images = RwSignal::new(false);
    let busy = RwSignal::new(false);
    // What the delete did, shown in place of the question: the images it
    // removed, the ones it kept and why — too much for toasts.
    let done = RwSignal::new(None::<BuildSetResponse>);
    let id = v.build.id;
    let name = if v.build.spec.name.is_empty() {
        v.build.spec.slug.clone()
    } else {
        v.build.spec.name.clone()
    };
    let name_sv = StoredValue::new(name.clone());
    let defaults: Vec<String> = v
        .used_by
        .iter()
        .filter(|u| {
            matches!(
                u.kind,
                ImageUseKind::ClassDefault | ImageUseKind::ModelOverride
            )
        })
        .map(super::use_label)
        .collect();
    let confirm = Callback::new(move |()| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        let args = BuildSetArgs {
            action: BuildSetAction::Delete,
            id: Some(id),
            delete_images: with_images.get_untracked(),
            ..Default::default()
        };
        let name = name_sv.get_value();
        spawn_local(async move {
            let res = api::build_set(&args).await;
            busy.set(false);
            match res {
                Ok(r) => {
                    bk.toasts.ok(format!("deleted '{name}'"));
                    bk.load_builds();
                    if delete_left_something_to_say(&r) {
                        done.try_set(Some(r));
                    } else {
                        open.try_set(false);
                    }
                }
                Err(e) => bk.toasts.err(format!("{name}: {e}")),
            }
        });
    });
    let defaults_line = (!defaults.is_empty()).then(|| defaults.join(", "));
    view! {
        <Show
            when=move || done.with(Option::is_none)
            fallback=move || {
                view! {
                    {move || done.get().map(|r| delete_result(&name_sv.get_value(), &r))}
                    <ModalFooter>
                        <button class="btn primary" on:click=move |_| open.set(false)>
                            "Close"
                        </button>
                    </ModalFooter>
                }
            }
        >
            <p>
                "Delete " <b>{name_sv.get_value()}</b>
                "? Its runs stay in the history, detached from the build, "
                "and its images stay on disk unless you remove them too."
            </p>
            {defaults_line
                .clone()
                .map(|d| {
                    view! {
                        <div class="notice warn">
                            "Its moving tag is in use: " {d}
                            ". Those keep the image they name, but nothing rebuilds it any more."
                        </div>
                    }
                })}
            <label class="check" style="margin-top:10px">
                <input
                    type="checkbox"
                    prop:checked=move || with_images.get()
                    on:change=move |ev| with_images.set(event_target_checked(&ev))
                />
                "Also delete its images (any image still in use is kept, and said so)"
            </label>
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| open.set(false)>
                    "Keep it"
                </button>
                <ConfirmButton
                    label="Delete build"
                    confirm=format!("Delete '{}'?", name_sv.get_value())
                    class="btn danger"
                    disabled=Signal::derive(move || busy.get())
                    on_confirm=confirm
                />
            </ModalFooter>
        </Show>
    }
}

/// Whether a delete's answer holds more than "done": an image removed or
/// kept, a cache cleaned up.
fn delete_left_something_to_say(r: &BuildSetResponse) -> bool {
    !r.removed_images.is_empty() || !r.kept.is_empty() || !r.removed_caches.is_empty()
}

/// The kept tags by reason, in the order the reasons first came: a dev
/// instance keeps every image for one reason, said once instead of per tag.
fn kept_by_reason(kept: &[KeptImage]) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for k in kept {
        match out.iter_mut().find(|(r, _)| *r == k.reason) {
            Some((_, tags)) => tags.push(k.tag.clone()),
            None => out.push((k.reason.clone(), vec![k.tag.clone()])),
        }
    }
    out
}

/// What a delete did, in the modal it was asked in.
fn delete_result(name: &str, r: &BuildSetResponse) -> impl IntoView {
    let removed = r.removed_images.clone();
    let kept = r.kept.clone();
    let kept_groups = kept_by_reason(&kept);
    let caches = r.removed_caches.clone();
    view! {
        <div class="wiz-ok" style="margin-top:0">{format!("Deleted '{name}'.")}</div>
        {(!removed.is_empty())
            .then(|| {
                view! {
                    <div class="bk-result">
                        <div class="mini-head">{format!("Removed ({})", removed.len())}</div>
                        {removed
                            .iter()
                            .map(|t| view! { <code class="mono-sm">{t.clone()}</code> })
                            .collect_view()}
                    </div>
                }
            })}
        {(!kept.is_empty())
            .then(|| {
                view! {
                    <div class="bk-result">
                        <div class="mini-head">{format!("Kept ({})", kept.len())}</div>
                        {kept_groups
                            .iter()
                            .map(|(reason, tags)| {
                                view! {
                                    <div class="bk-kept">
                                        <span class="dim">{reason.clone()}</span>
                                        {tags
                                            .iter()
                                            .map(|t| view! { <code class="mono-sm">{t.clone()}</code> })
                                            .collect_view()}
                                    </div>
                                }
                            })
                            .collect_view()}
                    </div>
                }
            })}
        {(!caches.is_empty())
            .then(|| {
                view! {
                    <div class="bk-result">
                        <div class="mini-head">
                            {format!("Its own compile caches, removed ({})", caches.len())}
                        </div>
                        {caches
                            .iter()
                            .map(|c| view! { <code class="mono-sm">{c.clone()}</code> })
                            .collect_view()}
                    </div>
                }
            })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::builds::ExtraChange;

    #[test]
    fn an_update_tooltip_adds_what_the_reasons_do_not_say() {
        let u = UpdateStatus {
            checked_at: String::new(),
            reasons: vec!["master +3 commits".into(), "PR #12 pushed".into()],
            ref_moved: true,
            extras: vec![
                // already a reason: not repeated
                ExtraChange {
                    label: "PR #12".into(),
                    change: "pushed".into(),
                },
                ExtraChange {
                    label: "fork/llama.cpp feature".into(),
                    change: "pushed".into(),
                },
            ],
            errors: vec!["check failed: rate limited".into()],
        };
        assert_eq!(
            update_tooltip(&u),
            "master +3 commits\nPR #12 pushed\nfork/llama.cpp feature: pushed\ncheck failed: rate limited"
        );
        assert_eq!(update_tooltip(&UpdateStatus::default()), "up to date");
    }

    #[test]
    fn a_reread_asked_for_while_one_is_out_follows_it_once() {
        let mut q = ReadQueue::default();
        assert!(q.ask(), "nothing out: the read goes now");
        assert!(q.out());
        // Asked twice more while it is out: one follow-up, not two.
        assert!(!q.ask());
        assert!(!q.ask());
        assert!(q.done(), "the follow-up is due");
        assert!(!q.out());
        assert!(q.ask());
        assert!(!q.done(), "nothing asked meanwhile: no follow-up");
        assert!(q.ask());
    }

    #[test]
    fn kept_tags_are_grouped_under_their_reason() {
        let k = |tag: &str, reason: &str| KeptImage {
            tag: tag.into(),
            reason: reason.into(),
        };
        let groups = kept_by_reason(&[
            k("a", "dev instance"),
            k("b", "in use by the chat class default"),
            k("c", "dev instance"),
        ]);
        assert_eq!(
            groups,
            vec![
                (
                    "dev instance".to_string(),
                    vec!["a".to_string(), "c".to_string()]
                ),
                (
                    "in use by the chat class default".to_string(),
                    vec!["b".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn a_delete_that_touched_images_says_so_in_place() {
        let mut r = BuildSetResponse::default();
        assert!(!delete_left_something_to_say(&r));
        r.kept.push(KeptImage {
            tag: "localhost/lmgw-llama-server:x".into(),
            reason: "dev instance".into(),
        });
        assert!(delete_left_something_to_say(&r));
        let caches = BuildSetResponse {
            removed_caches: vec!["lmgw-llama-cuda-x (1.0 GiB)".into()],
            ..Default::default()
        };
        assert!(delete_left_something_to_say(&caches));
    }
}
