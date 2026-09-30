//! The run log panel (container-builds design §9.1 "Run log panel") and the
//! after-run panel (§6).
//!
//! The log is a byte-offset tail of the run's log file (`build_run_log`),
//! polled only while its answer says `done: false`, and appended — never
//! re-fetched whole. The phase, the step and the percent come off the shared
//! jobs feed. When the run is over, the panel says how it ended and offers
//! what follows: make it current, verify it, recreate the containers still
//! on the old image (one `container apply` per model — never the group
//! apply), set it as a class default.

use std::time::Duration;

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::builds::{
    BuildPhase, BuildRun, BuildRunStatus, BuildView, ContainerImage, ImageUse, ImageUseKind,
    PromoteResponse,
};

use super::recreate::{use_targets, RecreateButton, UnmatchedNote};
use super::reports::VerifyView;
use super::{phase_label, sha7, status_chip, tag_only, use_bk, use_chips, LiveRun};
use crate::backends_api as api;
use crate::scope::Scope;
use crate::widgets::{ConfirmButton, Modal, ModalFooter, ModalSize};

/// What the panel shows.
#[derive(Clone, Debug, PartialEq)]
pub struct LogTarget {
    pub run_id: i64,
    /// `None` for a run whose build was deleted.
    pub build_id: Option<i64>,
    pub name: String,
    /// Set right after a Make current: who used the old image, for the
    /// recreate offer.
    pub promoted: Option<PromoteResponse>,
}

/// How often a live log is asked for more.
const POLL: Duration = Duration::from_millis(1000);

/// Lines a log opens on when its run is not live: how it ended, without
/// paging through a long build from the start. Said in the panel, with
/// **Load all** beside it.
const LOG_TAIL: usize = 500;

/// Where a tail answer starts in the file: its end less what it holds. `0`
/// is the whole log.
pub fn tail_start(next_offset: u64, text_len: usize) -> u64 {
    next_offset.saturating_sub(text_len as u64)
}

/// The log header's progress words for a live run. `step` is podman's
/// `STEP n/m` of the *current stage* (a multi-stage Dockerfile restarts it in
/// every stage), so it is said as such, with the stage when the feed names
/// it; the bar beside it is the whole run's percent. A compile's own percent
/// is the step's.
pub fn progress_words(l: &LiveRun) -> String {
    let d = &l.detail;
    let mut parts = vec![format!("phase {}", phase_label(d.phase))];
    if let Some(w) = &d.waiting_for {
        parts.push(format!("waiting for {w} — one build runs at a time"));
        return parts.join(" · ");
    }
    match (d.stage.as_deref(), d.step.as_deref()) {
        (Some(stage), Some(step)) => parts.push(format!("stage {stage}, step {step}")),
        (None, Some(step)) => parts.push(format!("step {step} of this stage")),
        (Some(stage), None) => parts.push(format!("stage {stage}")),
        (None, None) => {}
    }
    if d.phase == BuildPhase::Build {
        if let Some(c) = d.percent {
            parts.push(format!("compiling {c}%"));
        }
    }
    if let Some(p) = l.percent {
        parts.push(format!("{p}% overall"));
    }
    parts.join(" · ")
}

#[component]
pub fn RunLogModal() -> impl IntoView {
    let bk = use_bk();
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = bk.log.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && bk.log.with_untracked(Option::is_some) {
            bk.log.set(None);
        }
    });
    view! {
        <Modal open=open title="Build run" size=ModalSize::Wide fill=true>
            {move || bk.log.get().map(|t| view! { <RunLogBody t=t open=open/> })}
        </Modal>
    }
}

#[component]
fn RunLogBody(t: LogTarget, open: RwSignal<bool>) -> impl IntoView {
    let bk = use_bk();
    let run_id = t.run_id;
    let build_id = t.build_id;
    let name = StoredValue::new(t.name.clone());
    let chunks = RwSignal::new(Vec::<(u64, String)>::new());
    let offset = StoredValue::new(0u64);
    let done = RwSignal::new(false);
    let err = RwSignal::new(None::<String>);
    let in_flight = StoredValue::new(false);
    let kick = RwSignal::new(0u64);
    let follow = StoredValue::new(true);
    let pre: NodeRef<html::Pre> = NodeRef::new();
    let run = RwSignal::new(None::<BuildRun>);
    let view_ = RwSignal::new(None::<BuildView>);
    let promoted = RwSignal::new(t.promoted.clone());
    // The log shown starts this far into the file (it opened on its tail).
    let cut = RwSignal::new(None::<u64>);
    // Whether the first read went out: only it may ask for the tail.
    let asked = StoredValue::new(false);
    let scope = Scope::new();

    let live = Memo::new(move |_| {
        bk.live
            .with(|m| m.values().find(|l| l.detail.run_id == run_id).cloned())
    });
    // Only the job's identity: the Cancel button is not redrawn (and an armed
    // confirm not reset) on every progress tick.
    let live_job = Memo::new(move |_| live.with(|l| l.as_ref().map(|l| l.job_id)));

    let fetch = move || {
        if in_flight.get_value()
            || done.get_untracked()
            || err.with_untracked(Option::is_some)
            || !open.get_untracked()
        {
            return;
        }
        in_flight.set_value(true);
        let from = offset.get_value();
        // A run that is not live opens on its last lines; a live one streams
        // from the start.
        let tail = (!asked.get_value() && from == 0 && live.with_untracked(Option::is_none))
            .then_some(LOG_TAIL);
        asked.set_value(true);
        scope.spawn(async move {
            let res = match tail {
                Some(n) => api::build_run_log_tail(run_id, n).await,
                None => api::build_run_log(run_id, from).await,
            };
            in_flight.set_value(false);
            match res {
                Ok(c) => {
                    let start = match tail {
                        Some(_) => tail_start(c.next_offset, c.text.len()),
                        None => from,
                    };
                    if start > 0 && tail.is_some() {
                        cut.set(Some(start));
                    }
                    let more = !c.text.is_empty();
                    if more {
                        chunks.update(|v| v.push((start, c.text)));
                    }
                    offset.set_value(c.next_offset.max(from));
                    if c.done {
                        done.set(true);
                    } else if more {
                        // A backlog comes in pieces: ask again at once.
                        kick.update(|n| *n += 1);
                    }
                }
                Err(e) => err.set(Some(e.to_string())),
            }
        });
    };
    Effect::new(move |_| {
        kick.get();
        fetch();
    });
    let poll = StoredValue::new(None::<IntervalHandle>);
    if let Ok(h) = set_interval_with_handle(fetch, POLL) {
        poll.set_value(Some(h));
        on_cleanup(move || h.clear());
    }

    // The run row and its build's users: once now, and again when it ends.
    // Only the newest answer counts: `build_get` asks podman who uses what,
    // so the read made while the run was going can land after the one made
    // when it ended — and put "running" back (seen in the live pass on a
    // one-second up-to-date run).
    let final_gen = StoredValue::new(0u64);
    let load_final = move || {
        let Some(bid) = build_id else { return };
        // Also run from a timeout, which can fire after the panel closed.
        let Some(g) = final_gen.try_get_value().map(|g| g + 1) else {
            return;
        };
        final_gen.set_value(g);
        scope.spawn(async move {
            // `before` is exclusive: run_id + 1 lands exactly on this run.
            if let Ok(r) = api::build_get(bid, 1, Some(run_id + 1)).await {
                if final_gen.try_get_value() != Some(g) {
                    return;
                }
                run.set(r.runs.into_iter().find(|x| x.id == run_id));
                view_.set(Some(r.view));
            }
        });
    };
    load_final();
    Effect::new(move |prev: Option<bool>| {
        let d = done.get();
        if d && prev == Some(false) {
            load_final();
            bk.load_builds();
        }
        d
    });
    // The job left the feed before the log said done (or the log op is not
    // answering): the run row is the next best word on how it ended.
    Effect::new(move |prev: Option<bool>| {
        let now = live.with(Option::is_some);
        if prev == Some(true) && !now {
            load_final();
            kick.update(|n| *n += 1);
        }
        now
    });

    // Follow the tail unless the reader scrolled up — also when the pane
    // shrinks under it (the after-run panel appearing above the log).
    Effect::new(move |_| {
        chunks.track();
        done.track();
        live.with(Option::is_some);
        run.track();
        if follow.get_value() {
            request_animation_frame(move || {
                if let Some(p) = pre.get_untracked() {
                    p.set_scroll_top(p.scroll_height());
                }
            });
        }
    });
    let on_scroll = move |_| {
        if let Some(p) = pre.get_untracked() {
            let bottom = p.scroll_top() + p.client_height() >= p.scroll_height() - 8;
            follow.set_value(bottom);
        }
    };

    // Over when the run row says so: the log's `done` and the job leaving the
    // feed only say when to read the row again.
    let finished = move || {
        live.with(Option::is_none)
            && run.with(|r| r.as_ref().is_some_and(|r| r.status.is_terminal()))
    };
    // The log says done but the row read still says running (an answer from
    // before the end): read it again, a few times, a moment apart.
    let rereads = StoredValue::new(0u32);
    Effect::new(move |_| {
        let stale = done.get()
            && live.with(Option::is_none)
            && run.with(|r| r.as_ref().is_some_and(|r| !r.status.is_terminal()));
        if stale && rereads.get_value() < 5 {
            rereads.update_value(|n| *n += 1);
            set_timeout(load_final, Duration::from_millis(800));
        }
    });
    // The poll stops once there is nothing more to wait for: the log said
    // done, or — a run whose log never will (its job died) — the run row is
    // over and off the feed. A backlog still comes in through `kick`.
    Effect::new(move |_| {
        let row_over = run.with(|r| r.as_ref().map(|r| r.status.is_terminal()));
        if poll_over(done.get(), live.with(Option::is_some), row_over) {
            if let Some(h) = poll.try_update_value(Option::take).flatten() {
                h.clear();
            }
        }
    });
    let head_status = move || match (live.get(), run.get()) {
        (Some(l), _) => status_chip(BuildRunStatus::Running, Some(&l)),
        (None, Some(r)) => status_chip(r.status, None),
        (None, None) => ().into_any(),
    };
    let step_line = move || live.with(|l| l.as_ref().map(progress_words));
    // Also run from continuations of ops started below (Make current,
    // Verify, a recreate), which may answer after the panel closed: those
    // call it with `try_run`, a no-op once this body is gone.
    let reload = Callback::new(move |()| {
        load_final();
        bk.load_builds();
    });
    // The whole log, from its first byte, in place of the tail.
    let load_all = move |_| {
        chunks.set(Vec::new());
        offset.set_value(0);
        cut.set(None);
        err.set(None);
        done.set(false);
        kick.update(|n| *n += 1);
    };
    let copy = move |_| {
        let text: String = chunks.with(|c| c.iter().map(|(_, t)| t.as_str()).collect());
        crate::widgets::copy_secret(&text, bk.toasts, "log copied".to_string());
    };

    view! {
        <div class="logs-bar">
            <b>{name.get_value()}</b>
            <span class="dim mono-sm">{format!("run {run_id}")}</span>
            {head_status}
            <span class="dim mono-sm logs-ctr">{step_line}</span>
            <span class="spacer"></span>
        </div>
        {move || {
            live.get()
                .map(|l| {
                    let pct = l.percent;
                    view! {
                        <div class="progress bk-progress" title=pct.map(|p| format!("{p}%"))>
                            <i style=format!("width:{}%", pct.unwrap_or(2).min(100))></i>
                        </div>
                    }
                })
        }}
        {move || {
            err.get()
                .map(|e| {
                    view! {
                        <div class="notice err row bk-strip">
                            <span class="spacer">"The log did not load: " {e}</span>
                            <button
                                class="btn ghost sm"
                                on:click=move |_| {
                                    err.set(None);
                                    kick.update(|n| *n += 1);
                                }
                            >
                                "Retry"
                            </button>
                        </div>
                    }
                })
        }}
        <Show when=finished>
            {move || {
                run.get()
                    .map(|r| {
                        view! { <AfterRun run=r view=view_.get() promoted=promoted reload=reload/> }
                    })
            }}
        </Show>
        {move || {
            cut.get()
                .map(|_| {
                    view! {
                        <div class="dim mini-note row bk-strip">
                            <span class="spacer">
                                {format!("The last {LOG_TAIL} lines — how the run ended.")}
                            </span>
                            <button class="btn ghost sm" on:click=load_all>
                                "Load all"
                            </button>
                        </div>
                    }
                })
        }}
        <pre class="preset fill-pane logs-pre bk-log" node_ref=pre on:scroll=on_scroll>
            <For each=move || chunks.get() key=|c| c.0 let:c>
                {c.1}
            </For>
            {move || {
                chunks
                    .with(Vec::is_empty)
                    .then(|| if done.get() { "(the log is empty)" } else { "(waiting for output…)" })
            }}
        </pre>
        <ModalFooter>
            <button
                class="btn ghost"
                title=move || {
                    if cut.with(Option::is_some) {
                        "Copies the lines shown — Load all first for the whole log"
                    } else {
                        "Copies the whole log"
                    }
                }
                on:click=copy
            >
                "Copy log"
            </button>
            {move || {
                live_job
                    .get()
                    .map(|job| {
                        view! {
                            <ConfirmButton
                                label="Cancel run"
                                confirm="Cancel this run?"
                                class="btn danger"
                                title="Stop the build; the compile cache keeps what it compiled"
                                on_confirm=Callback::new(move |()| bk.cancel(job, name.get_value()))
                            />
                        }
                    })
            }}
        </ModalFooter>
    }
}

/// Whether a log panel can stop polling: the log said done, or the run row
/// (`row_over`: `Some(terminal)` once read) says it ended and the job has
/// left the feed — a run whose job died never writes its log's end.
pub fn poll_over(log_done: bool, on_feed: bool, row_over: Option<bool>) -> bool {
    log_done || (!on_feed && row_over == Some(true))
}

/// The running containers still on an older image of build `build_id` whose
/// model follows the moving tag — the §6 "Recreate N running containers" of
/// a run that moved the tag itself. `followers` are the config users of the
/// moving tag (class defaults, model overrides naming it): a container is
/// counted when its class's default is the tag, or its model's override is.
pub fn stale_followers(
    images: &[ContainerImage],
    build_id: i64,
    current_image: &str,
    followers: &[ImageUse],
) -> Vec<ImageUse> {
    let follows = |u: &ImageUse| {
        followers.iter().any(|c| match c.kind {
            ImageUseKind::ClassDefault => c.class == u.class,
            ImageUseKind::ModelOverride => {
                c.class == u.class && c.model_id.is_some() && c.model_id == u.model_id
            }
            ImageUseKind::RunningContainer | ImageUseKind::StoppedContainer => false,
        })
    };
    let mut out: Vec<ImageUse> = Vec::new();
    for img in images.iter().filter(|i| {
        i.id != current_image
            && i.provenance
                .as_ref()
                .is_some_and(|p| p.build_id == build_id)
    }) {
        for u in img
            .used_by
            .iter()
            .filter(|u| u.kind == ImageUseKind::RunningContainer && follows(u))
        {
            if !out.contains(u) {
                out.push(u.clone());
            }
        }
    }
    out
}

#[component]
fn AfterRun(
    run: BuildRun,
    view: Option<BuildView>,
    promoted: RwSignal<Option<PromoteResponse>>,
    reload: Callback<()>,
) -> impl IntoView {
    let bk = use_bk();
    let toasts = bk.toasts;
    let run_id = run.id;
    let status = run.status;
    let engine = run.engine;
    // The server's own fact when the build still exists (its `BuildView`,
    // loaded alongside this run) — "Set as default" must name the tag its
    // runs move, dev instance included. Only a run whose build was deleted
    // falls back to inferring it from the run's own tags.
    let moving = view
        .as_ref()
        .map(|v| v.moving_tag.clone())
        .unwrap_or_else(|| super::run_moving_tag(&run));
    let moving_sv = StoredValue::new(moving.clone());
    let verified = matches!(status, BuildRunStatus::Succeeded | BuildRunStatus::UpToDate);
    let has_image = run.image_id.is_some();
    let busy = RwSignal::new(false);

    // Current: made current here or by the run, or (an up-to-date run, which
    // builds nothing) its image is the one the moving tag names.
    let current_image = view
        .as_ref()
        .and_then(|v| v.current_run.as_ref())
        .and_then(|r| r.image_id.clone());
    let same_image = run.image_id.is_some() && run.image_id == current_image;
    let is_current = move || run.promoted || same_image || promoted.with(Option::is_some);
    // Who was on the image before: a Make current here answers it; after a
    // run that moved the tag itself, the running containers of this build's
    // older images whose model follows the tag.
    let stale = RwSignal::new(Vec::<ImageUse>::new());
    let scope = Scope::new();
    if let (true, Some(bid), Some(img)) = (
        verified && (run.promoted || same_image),
        run.build_id,
        run.image_id.clone(),
    ) {
        let followers = view.as_ref().map(|v| v.used_by.clone()).unwrap_or_default();
        scope.spawn(async move {
            if let Ok(r) = api::container_images(false).await {
                stale.set(stale_followers(&r.images, bid, &img, &followers));
            }
        });
    }
    let uses = {
        let from_view = view.as_ref().map(|v| v.used_by.clone()).unwrap_or_default();
        Memo::new(move |_| {
            promoted
                .with(|p| p.as_ref().map(|p| p.used_by.clone()))
                .unwrap_or_else(|| {
                    let mut u = from_view.clone();
                    for s in stale.get() {
                        if !u.contains(&s) {
                            u.push(s);
                        }
                    }
                    u
                })
        })
    };
    // Only what is still on another image: a running container of the
    // current image is where it should be already.
    let targets = use_targets(Signal::derive(move || {
        promoted
            .with(|p| p.as_ref().map(|p| p.used_by.clone()))
            .unwrap_or_else(|| stale.get())
    }));
    let missing_defaults = move || {
        uses.with(|u| {
            engine
                .classes()
                .iter()
                .copied()
                .filter(|c| {
                    !u.iter()
                        .any(|x| x.kind == ImageUseKind::ClassDefault && x.class == *c)
                })
                .collect::<Vec<&'static str>>()
        })
    };

    let make_current = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let res = api::build_promote(run_id).await;
            busy.set(false);
            match res {
                Ok(p) => {
                    toasts.ok(format!(
                        "{} now points at {}",
                        tag_only(&p.moving_tag),
                        sha7(&p.to_image)
                    ));
                    promoted.set(Some(p));
                    // The panel may be gone by now (closed while this was
                    // out): `try_run` is a no-op then.
                    reload.try_run(());
                }
                Err(e) => toasts.err(format!("make current: {e}")),
            }
        });
    };
    let verify = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let res = api::build_verify(run_id).await;
            busy.set(false);
            match res {
                Ok(r) => {
                    if r.gpu_verified {
                        toasts.ok("GPU-verified");
                    } else {
                        toasts.warn(format!("not GPU-verified — {}", r.notes.join("; ")));
                    }
                    reload.try_run(());
                }
                Err(e) => toasts.err(format!("verify: {e}")),
            }
        });
    };
    let set_default = move |class: &'static str| {
        let tag = moving_sv.get_value();
        spawn_local(async move {
            match api::set_class_default(class, &tag).await {
                Ok(v) => {
                    let note = v
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .map(|m| format!(" — {m}"))
                        .unwrap_or_default();
                    toasts.ok(format!("{class} default is now {tag}{note}"));
                    reload.try_run(());
                }
                Err(e) => toasts.err(format!("{class} default: {e}")),
            }
        });
    };

    let headline = move || {
        let image = promoted
            .with(|p| p.as_ref().map(|p| p.to_image.clone()))
            .or_else(|| run.image_id.clone());
        match status {
            BuildRunStatus::UpToDate if is_current() => format!(
                "Up to date: nothing was built — the current image{} has exactly these inputs and is verified.",
                image.map(|i| format!(" ({})", sha7(&i))).unwrap_or_default()
            ),
            BuildRunStatus::UpToDate => format!(
                "Up to date: nothing was built — an image with exactly these inputs{} exists and is verified, but the moving tag points at another. Make current moves it here.",
                image.map(|i| format!(" ({})", sha7(&i))).unwrap_or_default()
            ),
            _ if is_current() => format!(
                "{} now points at this run's image{}",
                moving_sv.get_value(),
                image.map(|i| format!(" ({})", sha7(&i))).unwrap_or_default()
            ),
            BuildRunStatus::Succeeded => {
                "Verified, but not current: the moving tag points at another run's image".to_string()
            }
            BuildRunStatus::Unverified => {
                "Built, but not GPU-verified, so not made current. Verify now can finish it — when the GPU is free."
                    .to_string()
            }
            BuildRunStatus::Broken => {
                "Built, but a verify check failed: not made current.".to_string()
            }
            BuildRunStatus::Failed => "The run did not produce an image.".to_string(),
            BuildRunStatus::Canceled => "Canceled before it produced an image.".to_string(),
            BuildRunStatus::Running => "Still running.".to_string(),
        }
    };
    let error = run.error.clone();
    let verify_json = run.verify.clone();

    view! {
        <div class="card bk-after">
            <div class="row">
                {status_chip(status, None)}
                <span class="spacer">{headline}</span>
            </div>
            {error.map(|e| view! { <div class="wiz-err">{e}</div> })}
            {verify_json.map(|v| view! { <VerifyView v=v/> })}
            {move || {
                uses.with(|u| {
                    (!u.is_empty())
                        .then(|| {
                            view! {
                                <div class="bk-chips">
                                    <span class="dim">"Used by"</span>
                                    {use_chips(u)}
                                </div>
                            }
                        })
                })
            }}
            <UnmatchedNote targets=targets/>
            <div class="row bk-after-acts">
                {(verified && has_image)
                    .then(|| {
                        view! {
                            <Show when=move || !is_current()>
                                <button
                                    class="btn primary"
                                    disabled=move || busy.get()
                                    title="Move the moving tag to this run's image"
                                    on:click=make_current
                                >
                                    "Make current"
                                </button>
                            </Show>
                        }
                    })}
                {(has_image && matches!(status, BuildRunStatus::Unverified | BuildRunStatus::Broken))
                    .then(|| {
                        view! {
                            <button
                                class="btn"
                                disabled=move || busy.get()
                                title="Run the --help and GPU device checks again"
                                on:click=verify
                            >
                                "Verify now"
                            </button>
                        }
                    })}
                <RecreateButton
                    targets=targets
                    offer=Signal::derive(is_current)
                    still_on="the previous image"
                    on_done=reload
                />
                {move || {
                    (is_current() && verified)
                        .then(|| {
                            missing_defaults()
                                .into_iter()
                                .map(|class| {
                                    view! {
                                        <button
                                            class="btn"
                                            title=format!(
                                                "Settings → Runtimes: {class} image = {}. Running {class} models keep their image until they restart.",
                                                moving_sv.get_value(),
                                            )
                                            on:click=move |_| set_default(class)
                                        >
                                            {format!("Set as default for {class}")}
                                        </button>
                                    }
                                })
                                .collect_view()
                        })
                }}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::builds::BuildRunJobDetail;

    fn live(detail: BuildRunJobDetail, percent: Option<u64>) -> LiveRun {
        LiveRun {
            job_id: 1,
            detail,
            percent,
        }
    }

    #[test]
    fn containers_on_an_older_image_are_stale_only_when_their_model_follows_the_tag() {
        use lmgw_api_types::builds::ImageProvenance;
        let running = |class: &str, model: &str| ImageUse {
            kind: ImageUseKind::RunningContainer,
            class: class.into(),
            model_id: Some(model.into()),
            container: Some(format!("lmgw-{class}-{model}")),
        };
        let img = |id: &str, build: i64, used: Vec<ImageUse>| ContainerImage {
            id: id.into(),
            provenance: Some(ImageProvenance {
                build_id: build,
                ..Default::default()
            }),
            used_by: used,
            ..Default::default()
        };
        let images = vec![
            img("new", 1, vec![running("chat", "fresh")]),
            img(
                "old",
                1,
                vec![
                    running("chat", "a"),
                    running("aux", "e"),
                    running("audio", "x"),
                ],
            ),
            img("older", 1, vec![running("chat", "a")]),
            img("other", 2, vec![running("chat", "b")]),
        ];
        let followers = vec![
            ImageUse {
                kind: ImageUseKind::ClassDefault,
                class: "chat".into(),
                ..Default::default()
            },
            ImageUse {
                kind: ImageUseKind::ModelOverride,
                class: "aux".into(),
                model_id: Some("e".into()),
                ..Default::default()
            },
        ];
        assert_eq!(
            stale_followers(&images, 1, "new", &followers),
            vec![running("chat", "a"), running("aux", "e")]
        );
        assert!(stale_followers(&images, 1, "new", &[]).is_empty());
    }

    #[test]
    fn a_tail_says_where_in_the_log_it_starts() {
        assert_eq!(tail_start(1000, 250), 750);
        // the whole log fitted in the tail
        assert_eq!(tail_start(250, 250), 0);
        assert_eq!(tail_start(0, 0), 0);
        assert_eq!(tail_start(10, 20), 0);
    }

    #[test]
    fn a_log_stops_polling_when_done_or_when_its_ended_row_is_off_the_feed() {
        assert!(poll_over(true, true, None));
        // live, or not read yet: keep asking
        assert!(!poll_over(false, true, Some(true)));
        assert!(!poll_over(false, false, None));
        assert!(!poll_over(false, false, Some(false)));
        // an orphaned run: the row ended, the job is gone, the log never says so
        assert!(poll_over(false, false, Some(true)));
    }

    #[test]
    fn a_step_is_said_as_the_current_stages_and_the_percent_as_the_whole_runs() {
        let d = BuildRunJobDetail {
            phase: BuildPhase::Build,
            step: Some("7/7".into()),
            ..Default::default()
        };
        assert_eq!(
            progress_words(&live(d.clone(), Some(14))),
            "phase build · step 7/7 of this stage · 14% overall"
        );
        let d = BuildRunJobDetail {
            stage: Some("2/6".into()),
            step: Some("11/13".into()),
            percent: Some(45),
            ..d
        };
        assert_eq!(
            progress_words(&live(d, Some(27))),
            "phase build · stage 2/6, step 11/13 · compiling 45% · 27% overall"
        );
        let w = BuildRunJobDetail {
            phase: BuildPhase::Waiting,
            waiting_for: Some("ik main".into()),
            ..Default::default()
        };
        assert_eq!(
            progress_words(&live(w, None)),
            "phase waiting · waiting for ik main — one build runs at a time"
        );
    }
}
