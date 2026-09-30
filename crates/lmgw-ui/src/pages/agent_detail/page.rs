use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};
use leptos_router::NavigateOptions;
use lmgw_api_types::{AgentDetail, AgentRunSummary};
use serde_json::Value;

use super::*;
use crate::widgets::schema_form::Draft;
use crate::widgets::{use_dirty_guard, ClampText, Density, NavTab, PageFrame, PageMode, SubNav};

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

/// The page's tabs, as the path segment after the agent (`/agents/<id>/runs`).
/// Run is the agent's own path.
///
/// The App tab is `ui`, not `app`: the gateway keeps `/agents/<id>/app*` for
/// the notice that an agent's UI moved to an origin of its own (origins
/// §4.2), so a reload of `/agents/<id>/app` would get that JSON, not this
/// page. `/agents/<id>/mcp` is the gateway's too (the agent's MCP face).
pub const TABS: &[&str] = &["run", "runs", "ui", "definition"];

/// Where a tab lives. An unknown tab is the Run tab, so an old or mistyped
/// link still lands on the agent; `app` is the old name of `ui`.
pub fn tab_href(id: &str, tab: &str) -> String {
    match tab {
        "runs" | "definition" => format!("/agents/{id}/{tab}"),
        "ui" | "app" => format!("/agents/{id}/ui"),
        _ => format!("/agents/{id}"),
    }
}

/// The address a request is moved to, if any: the `?tab=` links from before
/// tabs were paths (the catalog card's `app` chip, a bookmark) and the
/// spelled-out `/agents/<id>/run` land on the tab's own path, replacing the
/// history entry they came in on.
pub fn canonical_href(id: &str, segment: Option<&str>, query_tab: Option<&str>) -> Option<String> {
    if let Some(t) = query_tab {
        return Some(tab_href(id, t));
    }
    (segment == Some("run")).then(|| tab_href(id, "run"))
}

/// The batch runs of one agent, newest first, or the read's error.
pub(super) type RunList = RwSignal<Option<Result<Vec<AgentRunSummary>, String>>>;

#[component]
pub fn AgentDetailPage() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    let navigate = use_navigate();
    let id = Memo::new(move |_| params.with(|p| p.get("id").unwrap_or_default()));
    let tab = crate::url_state::use_view("tab", TABS, "run");
    Effect::new(move |_| {
        let id = id.get();
        let seg = params.with(|p| p.get("tab"));
        let q = query.with(|q| q.get("tab"));
        if let Some(href) = canonical_href(&id, seg.as_deref(), q.as_deref()) {
            navigate(
                &href,
                NavigateOptions {
                    replace: true,
                    ..Default::default()
                },
            );
        }
    });

    let detail = RwSignal::new(None::<AgentDetail>);
    // The service's bound mounts, kept beside the typed detail because they are
    // read out of the same document by hand (see [`service_mounts`]).
    let mounts = RwSignal::new(Vec::<ServiceMount>::new());
    let error = RwSignal::new(None::<String>);
    let draft = Draft::new();
    // The Definition tab's editor and its inline report live here, not inside
    // the tab: the tab body is re-rendered on every tab click *and* on every
    // re-read of the agent, so a manifest someone is halfway through writing —
    // and the warnings a save just came back with — would be thrown away by a
    // glance at the Run tab. Both are re-seeded by `load`, which is the only
    // thing allowed to overwrite them.
    let manifest = RwSignal::new(String::new());
    // The manifest as stored: the editor is unsaved while it differs.
    let manifest_base = RwSignal::new(String::new());
    let report = RwSignal::new(Vec::<String>::new());
    // The next load is of another agent (or the first): it replaces both
    // drafts. A re-read of the same agent keeps whichever holds edits.
    let reseed = StoredValue::new(true);
    // Same reason, and it matters more here: the rows of a run in flight and
    // the checkboxes a reviewer has been ticking cannot survive a tab click if
    // they live inside the tab.
    let run = RunState::new();
    // The Runs list is the page's too: its tab shows the count on every tab.
    let runs: RunList = RwSignal::new(None);

    let load = move || {
        let id = id.get_untracked();
        if id.is_empty() {
            return;
        }
        run.scope.spawn(async move {
            // As a document first, then as the DTO: the App tab's mount list
            // is read from the raw JSON (mounts §5.7), and taking both out of
            // one read keeps the page to a single request for one agent.
            match crate::api::get::<Value>(&format!("/api/agents/{id}")).await {
                Ok(doc) => {
                    mounts.set(service_mounts(&doc));
                    match serde_json::from_value::<AgentDetail>(doc) {
                        Ok(d) => {
                            let fresh = reseed.get_value();
                            reseed.set_value(false);
                            if fresh {
                                draft.load(&d.fields, &d.config);
                            } else {
                                draft.reload(&d.fields, &d.config);
                            }
                            // The stored manifest is canonicalized on the way
                            // in, so what is on screen after a save is what is
                            // actually stored — unless the editor holds edits
                            // of its own, which a re-read for anything else
                            // (a config save, the service) must not wipe.
                            let clean = manifest
                                .with_untracked(|m| manifest_base.with_untracked(|b| m == b));
                            if fresh || clean {
                                manifest.set(d.manifest.clone());
                            }
                            manifest_base.set(d.manifest.clone());
                            error.set(None);
                            detail.set(Some(d));
                        }
                        Err(e) => {
                            detail.set(None);
                            error.set(Some(format!("bad response body: {e}")));
                        }
                    }
                }
                Err(e) => {
                    detail.set(None);
                    error.set(Some(e.to_string()));
                }
            }
        });
    };
    // Re-reads when the agent changes, so /agents/a → /agents/b swaps content
    // rather than reusing the component's first load; a tab change keeps the
    // agent. The report is cleared here rather than in `load`, so it survives
    // the re-read a save triggers but never follows you to another agent.
    Effect::new(move |_| {
        id.track();
        report.set(Vec::new());
        run.clear();
        runs.set(None);
        reseed.set_value(true);
        load();
    });

    // The config form and the manifest are the page's two drafts; leaving
    // the agent with either unsaved asks (ux:U-3). Its tabs are paths under
    // /agents/<id> and keep both.
    let manifest_unsaved = Memo::new(move |_| manifest.with(|m| manifest_base.with(|b| m != b)));
    let config_unsaved = Memo::new(move |_| draft.changed().len());
    use_dirty_guard().watch_in(
        "this agent",
        Signal::derive(move || manifest_unsaved.get() || config_unsaved.get() > 0),
        Signal::derive(move || format!("/agents/{}", id.get())),
    );
    // The review's unticks and changed answers are the third: nothing holds
    // them but this page until Apply sends them (code:A1).
    let review_edited = Memo::new(move |_| run.edits() > 0);
    use_dirty_guard().watch_in(
        "this run's review",
        review_edited.into(),
        Signal::derive(move || format!("/agents/{}", id.get())),
    );

    // The run surface follows the jobs frame on `/api/events` (§6.2), matched
    // on the job **key** rather than on its detail: the key is set the moment
    // the row is claimed, so a run is followed from its first frame instead of
    // from its first progress report.
    let live = crate::live::use_live();
    let tick = Memo::new(move |_| {
        let key = format!("agent:{}", id.get());
        live.jobs
            .get()
            .unwrap_or_default()
            .into_iter()
            .find(|j| j.kind == "agent_run" && j.key.as_deref() == Some(key.as_str()))
            // `log_lines` is in the tuple because a container run can produce
            // nothing *but* log lines for minutes — a pull, a banner, its own
            // diagnostics — and `done` never moves for any of it
            // (container-runtime §3.2). It is monotonic, so it only ever means
            // "there is more log to read".
            .map(|j| {
                (
                    j.id,
                    j.done,
                    j.status.clone(),
                    j.detail.get("log_lines").and_then(Value::as_u64),
                )
            })
    });
    // A memo only fires on change, so this re-reads the rows on every change of
    // `done` or of the log-line count, and on nothing else. The rows and the
    // log themselves never ride the jobs feed (§3) — the frame is the trigger,
    // this GET is the pipe, and `RunState::load` keeps one of them in flight at
    // a time.
    Effect::new(move |prev: Option<(String, Option<Tick>)>| {
        let agent = id.get();
        let now = tick.get();
        // The router keeps this page for the next agent: the last tick of the
        // agent shown before is not "this agent's job just finished", and
        // reading it would put that agent's run on this one's page.
        let prev = prev.filter(|(a, _)| *a == agent).map(|(_, t)| t);
        // A run the owner opened from the Runs tab keeps the surface: a frame
        // from a *different* run must not drag them off the one they asked
        // for. Its own frames still land, so reopening a live run follows it.
        let pinned = run.pinned.get_untracked();
        if let Some(pin) = pinned {
            if now.as_ref().map(|(id, _, _, _)| *id) != Some(pin) {
                return (agent, now);
            }
        }
        match (&prev, &now) {
            (_, Some((job_id, _, _, _))) => run.load(*job_id),
            // The job left the live list, which is how a consumer learns it
            // finished: read it once more for the stored result, and re-read
            // the agent so its "last run" is current.
            (Some(Some((job_id, _, _, _))), None) => {
                run.load(*job_id);
                load();
            }
            _ => {}
        }
        (agent, now)
    });

    // A run already in flight when the page opens is picked up straight away,
    // rather than only at the next frame.
    Effect::new(move |seen: Option<Option<i64>>| {
        let live_job = detail.get().and_then(|d| d.live_job).map(|j| j.job_id);
        if let Some(job_id) = live_job {
            if seen.flatten() != Some(job_id) {
                run.load(job_id);
            }
        }
        live_job
    });

    // The durable run list is re-read when a run of this agent starts or ends,
    // never on a timer — the same trigger Downloads and Docs use. A chat agent
    // has threads instead, which its tab reads itself.
    let batchy = Memo::new(move |_| detail.with(|d| d.as_ref().map(|d| d.kind != "chat")));
    let run_ticks = Memo::new(move |_| {
        let key = format!("agent:{}", id.get());
        live.jobs
            .get()
            .unwrap_or_default()
            .into_iter()
            .filter(|j| j.kind == "agent_run" && j.key.as_deref() == Some(key.as_str()))
            .map(|j| (j.id, j.status.clone()))
            .collect::<Vec<_>>()
    });
    let load_runs = Callback::new(move |()| {
        let want = id.get_untracked();
        run.scope.spawn(async move {
            let res =
                crate::api::get::<Vec<AgentRunSummary>>(&format!("/api/agents/{want}/runs")).await;
            // A read for the agent the page has since left is dropped.
            if id.get_untracked() == want {
                runs.set(Some(res.map_err(|e| e.to_string())));
            }
        });
    });
    Effect::new(move |_| {
        run_ticks.track();
        if batchy.get() == Some(true) {
            load_runs.run(());
        }
    });

    let reload = Callback::new(move |()| load());

    let title = move || {
        detail
            .with(|d| d.as_ref().map(|d| d.name.clone()))
            .unwrap_or_else(|| "Agent".to_string())
    };
    let tabs = Signal::derive(move || {
        detail.with(|d| {
            let Some(d) = d else { return Vec::new() };
            let id = d.id.as_str();
            let mut v = vec![NavTab::new("Run", tab_href(id, "run")).exact()];
            if d.kind == "chat" {
                v.push(NavTab::new("Threads", tab_href(id, "runs")).count(d.threads));
            } else {
                let mut t = NavTab::new("Runs", tab_href(id, "runs"));
                if let Some(n) =
                    runs.with(|r| r.as_ref().and_then(|r| r.as_ref().ok()).map(Vec::len))
                {
                    t = t.count(n);
                }
                v.push(t);
            }
            // Only for an agent that declares one (container-runtime §3.3):
            // no `service`, no proxy route and nothing to show.
            if d.service.is_some() {
                v.push(NavTab::new("App", tab_href(id, "ui")));
            }
            v.push(NavTab::new("Definition", tab_href(id, "definition")));
            v
        })
    });
    // Identity, kind badges and the tabs, once the detail is in.
    let head_extra = move || {
        view! {
            {move || {
                detail
                    .with(|d| {
                        d.as_ref()
                            .map(|d| {
                                view! {
                                    <span class="sub mono-sm">{d.id.clone()}</span>
                                    <span class="type-badge">{d.kind.clone()}</span>
                                    {(d.source == "builtin")
                                        .then(|| view! { <span class="type-badge">"shipped"</span> })}
                                    {(!d.enabled)
                                        .then(|| view! { <span class="chip off">"disabled"</span> })}
                                }
                            })
                    })
            }}
            <Show when=move || detail.with(Option::is_some)>
                <SubNav tabs=tabs/>
            </Show>
        }
    };
    // The description, once, above every tab — folded to two lines, because
    // it is read once and then only looked up. Memoized, so a re-read of the
    // agent does not fold it again under the reader.
    let desc = Memo::new(move |_| {
        detail.with(|d| {
            d.as_ref()
                .map(|d| d.description.trim().to_string())
                .filter(|s| !s.is_empty())
        })
    });
    let row_error = Memo::new(move |_| detail.with(|d| d.as_ref().and_then(|d| d.error.clone())));
    let toolbar = move || {
        view! {
            {move || desc.get().map(|t| view! { <ClampText text=t lines=2 class="page-desc dim"/> })}
            // A row this build cannot read still opens (container-runtime
            // §4.3): the detail comes back with `error` set instead of a 400,
            // so the Definition editor stays reachable for the very manifest
            // that needs fixing.
            {move || {
                row_error
                    .get()
                    .map(|e| {
                        view! {
                            <div class="notice err">
                                <b>{e}</b>
                                <span class="detail">
                                    "The Definition tab has the stored text as it is; nothing else on "
                                    "this page can be trusted until it parses."
                                </span>
                            </div>
                        }
                    })
            }}
        }
    };

    view! {
        <PageFrame
            title=title
            mode=PageMode::Fill
            density=Density::Dense
            class="agent-page"
            head_extra=head_extra
            toolbar=toolbar
            actions=|| view! { <a class="btn ghost" href="/agents">"All agents"</a> }
        >
            <Show when=move || error.get().is_some()>
                <div class="notice err">{move || error.get().unwrap_or_default()}</div>
            </Show>
            <Show when=move || detail.with(Option::is_none) && error.get().is_none()>
                <div class="dim">"Loading…"</div>
            </Show>

            {move || {
                detail
                    .get()
                    .map(|d| {
                        // Every tab fills the body: each brings its own one
                        // scroller (a form column, a table, the manifest, the
                        // app's frame).
                        view! {
                            {move || match tab.get() {
                                "runs" => {
                                    view! {
                                        <RunsTab d=d.clone() run=run runs=runs reload_runs=load_runs/>
                                    }
                                        .into_any()
                                }
                                "ui" => {
                                    view! { <AppTab d=d.clone() mounts=mounts reload=reload/> }
                                        .into_any()
                                }
                                "definition" => {
                                    view! {
                                        <DefinitionTab
                                            d=d.clone()
                                            text=manifest
                                            base=manifest_base
                                            lines=report
                                            reload=reload
                                        />
                                    }
                                        .into_any()
                                }
                                _ => {
                                    view! {
                                        <RunTab d=d.clone() draft=draft reload=reload run=run runs=runs/>
                                    }
                                        .into_any()
                                }
                            }}
                        }
                    })
            }}
        </PageFrame>
    }
}
