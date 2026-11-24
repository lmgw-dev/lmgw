//! The detail pane (modern-sample.html): a header bar with the selected
//! entry's actions, and under it what the entry is.
//!
//! - A **build**: Run (Log while a run is live) and Edit in the header, the
//!   rest in its menu; the hero (the name, the repository, the state of the
//!   live or the last run, the update and engine chips); the live run as a
//!   card with its phase, its progress and podman's last line; then its
//!   source, its image, its update state and its runs as boxed groups, and
//!   Delete. The editor is still the modal it was.
//! - **Local images** and **Release builds**: their tables, each in its box.
//! - Nothing: the overview, what a build is and where to start one.
//!
//! Each entry's detail is its own component, so what it read for one build
//! (its runs) is dropped with it when another is selected.

use leptos::prelude::*;
use lmgw_api_types::builds::{BuildRunStatus, BuildView, GpuBackend};

use super::builds::{duplicate, update_flags, RunsGroup, Starts, UpdateGroup};
use super::list::{build_state, display_name};
use super::route::Sel;
use super::run_log::progress_words;
use super::{
    ago, engine_label, extra_short, local_ts, run_tag, sha7, status_chip, tag_only, use_bk,
    use_chips, Bk, EditorSeed,
};
use crate::widgets::nav_split::{track_scroll, DetailHead};
use crate::widgets::{ActionRow, CopyBtn, Ico, Icon, InfoRow, MenuItem, PrefGroup, StackRow};

#[component]
pub fn DetailPane(sel: Memo<Option<Sel>>, narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let bk = use_bk();
    // A build the list does not have (deleted, or a link to one that never
    // was) says so; while the list is not known yet (or failed to load) the
    // pane shows that instead of a build.
    let known = Memo::new(move |_| match sel.get() {
        Some(Sel::Build(id)) => bk
            .builds
            .with(|b| b.as_ref().map(|b| b.iter().any(|v| v.build.id == id))),
        _ => Some(true),
    });
    // The shown build went away (its Delete): back to the list.
    Effect::new(move |prev: Option<(Option<Sel>, Option<bool>)>| {
        let now = (sel.get(), known.get());
        if let Some((was, Some(true))) = prev {
            if was == now.0 && now.1 == Some(false) {
                back.run(());
            }
        }
        now
    });
    move || {
        if known.get() == Some(false) {
            return view! { <Gone narrow=narrow back=back/> }.into_any();
        }
        if known.get().is_none() {
            return view! { <Unknown narrow=narrow back=back/> }.into_any();
        }
        match sel.get() {
            None => view! { <Overview narrow=narrow back=back/> }.into_any(),
            Some(Sel::Build(id)) => {
                view! { <BuildDetail id=id narrow=narrow back=back/> }.into_any()
            }
            Some(Sel::Images) => view! { <ImagesDetail narrow=narrow back=back/> }.into_any(),
            Some(Sel::Windows) => view! { <WindowsDetail narrow=narrow back=back/> }.into_any(),
        }
    }
}

/// A build is selected but the list is not known: still loading, or it
/// failed to load (then it says so, with a Retry).
#[component]
fn Unknown(narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let bk = use_bk();
    let scrolled = RwSignal::new(false);
    view! {
        <DetailHead
            narrow=narrow
            back=back
            title=Signal::stored("Build".to_string())
            scrolled=scrolled
        />
        <div class="mdetail-empty">
            {move || match bk.builds_err.get() {
                Some(e) => {
                    view! {
                        <p>"Loading the builds failed: " {e}</p>
                        <button class="btn sm" on:click=move |_| bk.load_builds()>"Retry"</button>
                    }
                        .into_any()
                }
                None => view! { <p>"Loading…"</p> }.into_any(),
            }}
        </div>
    }
}

#[component]
fn Gone(narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    view! {
        <DetailHead
            narrow=narrow
            back=back
            title=Signal::stored("Not found".to_string())
            scrolled=scrolled
        />
        <div class="mdetail-empty">
            <p>"This build is not on this gateway (any more). Pick another in the list."</p>
        </div>
    }
}

/// Nothing selected: the counts, and where to start a build.
#[component]
fn Overview(narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let bk = use_bk();
    let bus = crate::live::use_live();
    let scrolled = RwSignal::new(false);
    let counts = move || {
        let n = bk.builds.with(|b| b.as_ref().map_or(0, Vec::len));
        let running = bk.live.with(|m| m.len());
        let upd = bus
            .updates
            .with(|u| u.as_ref().map_or(0, |u| u.builds_with_updates));
        (n, running, upd)
    };
    let last_check = move || {
        bus.updates
            .with(|u| u.as_ref().and_then(|u| u.checked_at.clone()))
            .map(|at| format!("{} ({})", ago(&at), local_ts(&at)))
            .unwrap_or_else(|| "not yet".to_string())
    };
    let checking = move || bk.checking.with(|c| c.contains(&0));
    view! {
        <DetailHead
            narrow=narrow
            back=back
            title=Signal::stored("Engines".to_string())
            scrolled=scrolled
        />
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="detail-flow">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Layers/>
                    </div>
                    <div class="hero-text">
                        <h1>"Engines"</h1>
                        <div class="sub">
                            "The container images lmgw's engines run in: built here from git, or pulled from a registry."
                        </div>
                    </div>
                </div>
                {usage_notice(bk)}
                <PrefGroup title="Builds">
                    <InfoRow title="Configured" value=move || counts().0.to_string()/>
                    <InfoRow title="Running now" value=move || counts().1.to_string()/>
                    <InfoRow title="With newer inputs" value=move || counts().2.to_string()/>
                    <ActionRow
                        title="Check for updates now"
                        sub=move || format!("Every build's ref and unpinned extras, and the registry images in use. Last full check: {}", last_check())
                    >
                        <button
                            class="btn"
                            disabled=checking
                            on:click=move |_| bk.check_updates(None, "update check".to_string())
                        >
                            {move || if checking() { "Checking…" } else { "Check now" }}
                        </button>
                    </ActionRow>
                </PrefGroup>
                <Starts/>
            </div>
        </div>
    }
}

/// Who uses which image could not be told: every "used by" is empty for
/// that reason, and the page says so.
fn usage_notice(bk: Bk) -> impl IntoView {
    move || {
        bk.usage_err.get().map(|e| {
            view! {
                <div class="notice warn bk-notice">
                    <b>"Who uses which image could not be told. "</b>
                    {format!(
                        "{e}. Every \u{201c}used by\u{201d} on this page is empty for that reason, not because nothing uses the image."
                    )}
                </div>
            }
        })
    }
}

fn backend_words(v: &BuildView) -> String {
    let s = &v.build.spec;
    let mut w = match s.backend {
        GpuBackend::Cuda => "CUDA".to_string(),
        GpuBackend::Vulkan => "Vulkan".to_string(),
        GpuBackend::Rocm => "ROCm".to_string(),
        GpuBackend::Cpu => "CPU".to_string(),
    };
    if let Some(c) = s
        .cuda_version
        .as_ref()
        .filter(|_| s.backend == GpuBackend::Cuda)
    {
        w.push_str(&format!(" {c}"));
    }
    match s.arch.as_deref() {
        Some(a) if !a.is_empty() => w.push_str(&format!(" · arch {}", a.join(", "))),
        _ => {}
    }
    w
}

/// The build's menu: what the header's two buttons leave.
fn build_menu(
    bk: Bk,
    id: i64,
    v: Memo<BuildView>,
    name: Memo<String>,
    live_job: Memo<Option<i64>>,
) -> Signal<Vec<MenuItem>> {
    Signal::derive(move || {
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
            MenuItem::new("Duplicate", move || duplicate(bk, id, name.get_untracked()))
                .title("A copy to change: the quick way to a variant with one more PR"),
            MenuItem::new("Check merge", move || {
                bk.merge.set(Some((id, name.get_untracked())))
            })
            .title("Fetch and merge every extra onto the base in a throwaway worktree; nothing is built"),
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
    })
}

#[component]
fn BuildDetail(id: i64, narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let bk = use_bk();
    // A build that just left the list keeps its last view until the pane
    // goes back to the list.
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
    let live = Memo::new(move |_| bk.live.with(|m| m.get(&id).cloned()));
    // The job alone, for what must not be redrawn on every progress tick.
    let live_job = Memo::new(move |_| live.with(|l| l.as_ref().map(|l| l.job_id)));
    let is_live = Memo::new(move |_| live_job.get().is_some());
    let scrolled = RwSignal::new(false);
    let more = build_menu(bk, id, v, name, live_job);
    let edit = move |_| {
        bk.ensure_env();
        bk.editor.set(Some(v.with_untracked(EditorSeed::edit)));
    };
    let actions = move || {
        let primary = move || {
            if live_job.get().is_some() {
                view! {
                    <button
                        type="button"
                        class="btn flat"
                        title="The live log, the phase and Cancel"
                        on:click=move |_| v.with_untracked(|v| bk.open_log(v))
                    >
                        <Icon ico=Ico::List/>
                        "Log"
                    </button>
                }
                .into_any()
            } else {
                view! {
                    <button
                        type="button"
                        class="btn flat"
                        title="Fetch, merge, build, verify and tag; skipped when these exact inputs already have a verified image"
                        on:click=move |_| bk.run(id, name.get_untracked(), false)
                    >
                        <Icon ico=Ico::Play/>
                        "Run"
                    </button>
                }
                .into_any()
            }
        };
        view! {
            {primary}
            <button type="button" class="btn flat" title="Change the build's definition" on:click=edit>
                <Icon ico=Ico::Sliders/>
                "Edit"
            </button>
        }
    };

    let chips = move || {
        let l = live.get();
        let last = v.with(|v| v.last_run.clone());
        let status = match (l.as_ref(), last.as_ref()) {
            (Some(l), _) => status_chip(BuildRunStatus::Running, Some(l)),
            (None, Some(r)) => status_chip(r.status, None),
            (None, None) => view! {
                <span class="chip">
                    <span class="dot stop"></span>
                    "never run"
                </span>
            }
            .into_any(),
        };
        let when = last
            .map(|r| {
                r.finished_at
                    .clone()
                    .unwrap_or_else(|| r.started_at.clone())
            })
            .map(|w| {
                let title = local_ts(&w);
                view! { <span class="chip" title=title>{ago(&w)}</span> }
            });
        let engine = v.with(|v| {
            let e = v.build.spec.engine;
            let title = format!("serves: {}", e.classes().join(", "));
            view! { <span class="chip" title=title>{engine_label(e)}</span> }
        });
        let flags = v.with(|v| v.update.as_ref().map(update_flags));
        view! { {status} {when} {engine} {flags} }
    };

    let spec = move |f: fn(&BuildView) -> String| move || v.with(f);
    let extras = move || v.with(|v| v.build.spec.extras.clone());
    let current_tag = Memo::new(move |_| v.with(|v| v.current_run.as_ref().and_then(run_tag)));
    let uses = move || v.with(|v| v.used_by.clone());

    view! {
        <DetailHead narrow=narrow back=back title=name scrolled=scrolled actions=actions more=more/>
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="detail-flow bk-detail-flow">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Layers/>
                    </div>
                    <div class="hero-text">
                        <h1 title=move || name.get()>{move || name.get()}</h1>
                        <div class="sub mono-sub" title=spec(|v| v.build.spec.repo_url.clone())>
                            {spec(|v| v.build.spec.repo_url.clone())}
                        </div>
                        <div class="chips">{chips}</div>
                    </div>
                </div>
                {usage_notice(bk)}
                {move || {
                    is_live.get().then(|| view! { <LiveRunGroup id=id v=v/> })
                }}
                <PrefGroup title="Source">
                    <InfoRow title="Engine" value=spec(|v| engine_label(v.build.spec.engine).to_string())/>
                    <InfoRow title="Repository" value=spec(|v| v.build.spec.repo_url.clone()) mono=true/>
                    <InfoRow title="Ref" value=spec(|v| v.build.spec.git_ref.clone()) mono=true/>
                    {move || {
                        let ex = extras();
                        if ex.is_empty() {
                            view! { <InfoRow title="Extras" value="none"/> }.into_any()
                        } else {
                            view! {
                                <StackRow title="Extras" sub="Merged on top of the ref, in this order">
                                    <div class="bk-chips wrap">
                                        {ex
                                            .iter()
                                            .map(|e| {
                                                let pin = e.pin().map(|p| format!(" @{}", sha7(p)));
                                                let pinned = pin.is_some();
                                                let title = format!(
                                                    "{}{}",
                                                    e.label(),
                                                    e.pin().map(|p| format!(" @ {p}")).unwrap_or_default(),
                                                );
                                                view! {
                                                    <span class="type-badge" class:bk-pinned=pinned title=title>
                                                        {extra_short(e)}
                                                        {pin}
                                                    </span>
                                                }
                                            })
                                            .collect_view()}
                                    </div>
                                </StackRow>
                            }
                                .into_any()
                        }
                    }}
                    <InfoRow title="GPU backend" value=spec(backend_words)/>
                    {move || {
                        let notes = v.with(|v| v.build.spec.notes.clone());
                        (!notes.trim().is_empty())
                            .then(|| {
                                view! {
                                    <StackRow title="Notes">
                                        <div class="bk-notes">{notes}</div>
                                    </StackRow>
                                }
                            })
                    }}
                </PrefGroup>
                <PrefGroup title="Image">
                    {move || {
                        let m = v.with(|v| v.moving_tag.clone());
                        view! {
                            <StackRow
                                title="Moving tag"
                                sub="Picking this tag in a class or model image field follows the build"
                            >
                                <div class="bk-tag-line">
                                    <code class="mono-sm bk-tag">{m.clone()}</code>
                                    <CopyBtn text=m/>
                                </div>
                            </StackRow>
                        }
                    }}
                    {move || {
                        match current_tag.get().as_deref().map(|t| tag_only(t).to_string()) {
                            Some(t) => {
                                view! {
                                    <StackRow title="Current image">
                                        <div class="bk-tag-line">
                                            <code class="mono-sm bk-tag">{t.clone()}</code>
                                            <CopyBtn text=t/>
                                        </div>
                                    </StackRow>
                                }
                                    .into_any()
                            }
                            None => {
                                view! { <InfoRow title="Current image" value="not built yet"/> }
                                    .into_any()
                            }
                        }
                    }}
                    {move || {
                        let uses = uses();
                        if uses.is_empty() {
                            view! { <InfoRow title="Used by" value="nothing uses its moving tag"/> }
                                .into_any()
                        } else {
                            view! {
                                <StackRow
                                    title="Used by"
                                    sub="Class defaults, model overrides and containers (running or stopped) on its moving tag"
                                >
                                    <div class="bk-chips wrap">{use_chips(&uses)}</div>
                                </StackRow>
                            }
                                .into_any()
                        }
                    }}
                </PrefGroup>
                <UpdateGroup id=id v=v name=name/>
                <RunsGroup id=id v=v name=name/>
                <PrefGroup>
                    <ActionRow
                        title="Delete build…"
                        sub=move || {
                            if live_job.get().is_some() {
                                "Cancel the run first".to_string()
                            } else {
                                "Delete the build; its runs stay in the history".to_string()
                            }
                        }
                        danger=true
                        on_click=Callback::new(move |()| {
                            if live_job.get_untracked().is_none() {
                                bk.deleting.set(Some(v.get_untracked()));
                            }
                        })
                    />
                </PrefGroup>
            </div>
        </div>
    }
}

/// The run going on now, as a card: its phase and step, the whole run's
/// progress, podman's last line, and the log and Cancel.
#[component]
fn LiveRunGroup(id: i64, v: Memo<BuildView>) -> impl IntoView {
    let bk = use_bk();
    let live = Memo::new(move |_| bk.live.with(|m| m.get(&id).cloned()));
    let job = Memo::new(move |_| live.with(|l| l.as_ref().map(|l| l.job_id)));
    let words = move || live.with(|l| l.as_ref().map(progress_words).unwrap_or_default());
    let pct = move || live.with(|l| l.as_ref().and_then(|l| l.percent));
    let last_line = move || {
        live.with(|l| {
            l.as_ref()
                .map(|l| match &l.detail.waiting_for {
                    Some(w) => format!("waiting for {w} (one build runs at a time)"),
                    None => l.detail.last_line.clone(),
                })
                .unwrap_or_default()
        })
    };
    let state = move || v.with(|v| build_state(v, live.get().as_ref())).1;
    view! {
        <div class="bk-wide">
            <PrefGroup title="Run in progress">
                <div class="prow stack block bk-live">
                    <div class="bk-live-head">
                        <span class="bk-live-words">
                            <b>{state}</b>
                            " · "
                            <span class="mono-sm">{words}</span>
                        </span>
                        <span class="bk-live-acts">
                            <button
                                type="button"
                                class="btn sm"
                                on:click=move |_| v.with_untracked(|v| bk.open_log(v))
                            >
                                "Open the log"
                            </button>
                            {move || {
                                job.get()
                                    .map(|j| {
                                        view! {
                                            <button
                                                type="button"
                                                class="btn sm danger"
                                                title="Stop the build; the compile cache keeps what it compiled"
                                                on:click=move |_| bk.cancel(j, v.with_untracked(display_name))
                                            >
                                                "Cancel run"
                                            </button>
                                        }
                                    })
                            }}
                        </span>
                    </div>
                    <div class="progress bk-progress" title=move || pct().map(|p| format!("{p}%"))>
                        <i style=move || format!("width:{}%", pct().unwrap_or(2).min(100))></i>
                    </div>
                    <pre class="bk-last-line" title=last_line>{last_line}</pre>
                </div>
            </PrefGroup>
        </div>
    }
}

/// The detail of a table entry: the header, then the table's own view on
/// the wide flow.
#[component]
fn TableDetail(
    narrow: Memo<bool>,
    back: Callback<()>,
    title: &'static str,
    sub: &'static str,
    glyph: Ico,
    #[prop(into)] actions: ViewFn,
    children: Children,
) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    view! {
        <DetailHead
            narrow=narrow
            back=back
            title=Signal::stored(title.to_string())
            scrolled=scrolled
            actions=actions
        />
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="bk-table-flow">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=glyph/>
                    </div>
                    <div class="hero-text">
                        <h1>{title}</h1>
                        <div class="sub">{sub}</div>
                    </div>
                </div>
                {children()}
            </div>
        </div>
    }
}

#[component]
fn ImagesDetail(narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let bk = use_bk();
    // The images are read the first time they are shown, and from then on
    // with the builds.
    if !bk.images_wanted.get_untracked() {
        bk.load_images();
    }
    view! {
        <TableDetail
            narrow=narrow
            back=back
            title="Local images"
            sub="Every llama.cpp, audio.cpp and sd.cpp image on this machine, built here or pulled"
            glyph=Ico::Layers
            actions=move || {
                view! {
                    <button
                        type="button"
                        class="icon-btn"
                        title="List the images again"
                        aria-label="List the images again"
                        on:click=move |_| bk.load_images()
                    >
                        <Icon ico=Ico::Refresh/>
                    </button>
                }
            }
        >
            {usage_notice(bk)}
            <super::images::ImagesTab/>
        </TableDetail>
    }
}

#[component]
fn WindowsDetail(narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let bk = use_bk();
    view! {
        <TableDetail
            narrow=narrow
            back=back
            title="Release builds"
            sub="The pinned engine releases lmgw downloads and runs natively"
            glyph=Ico::Download
            actions=move || {
                view! {
                    <button
                        type="button"
                        class="icon-btn"
                        title="Read the release builds again"
                        aria-label="Read the release builds again"
                        on:click=move |_| bk.load_releases()
                    >
                        <Icon ico=Ico::Refresh/>
                    </button>
                }
            }
        >
            <super::releases::ReleasesTab/>
        </TableDetail>
    }
}
