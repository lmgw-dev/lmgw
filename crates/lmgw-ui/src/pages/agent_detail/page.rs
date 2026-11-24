use leptos::prelude::*;
use leptos_router::hooks::{use_location, use_navigate, use_params_map, use_query_map};
use leptos_router::NavigateOptions;
use lmgw_api_types::{AgentDetail, AgentRunSummary};
use serde_json::Value;

use super::*;
use crate::pages::agents::{list_query, AgentsCtx};
use crate::scope::Latest;
use crate::widgets::nav_split::DetailHead;
use crate::widgets::schema_form::Draft;
use crate::widgets::{use_dirty_guard, NavTab, SubNav};

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

/// The reads of the selected agent: which one is the newest, and which agent
/// the page's drafts were last filled from.
///
/// The page stays mounted while the list's selection moves, so two quick
/// moves (to b, then to c) have two reads out at once, and they can land in
/// either order. Only the newest read's answer is applied, and only while
/// its agent is still the one selected. Whether an answer fills the drafts
/// afresh (the first read of a selection) or keeps what they hold (a re-read
/// after a save, a run's end) is decided per answer, from the agent it is of.
#[derive(Clone, Copy)]
pub(super) struct DetailReads {
    latest: Latest,
    seeded: StoredValue<Option<String>>,
}

impl DetailReads {
    #[allow(clippy::new_without_default)] // made where its owner is; not a value
    pub(super) fn new() -> Self {
        Self {
            latest: Latest::new(),
            seeded: StoredValue::new(None),
        }
    }

    /// A ticket for a read about to go out; `None` once the page is gone.
    pub(super) fn begin(self) -> Option<u64> {
        self.latest.next()
    }

    /// The selection moved: the next answer fills the drafts afresh, even if
    /// it is of the agent filled from before (a, b, back to a).
    pub(super) fn forget(self) {
        self.seeded.set_value(None);
    }

    /// An answer of agent `of` landing while `selected` is selected: `None`
    /// drops it (a newer read is out, or the selection has moved on), else
    /// whether it is the first of its selection, which fills the drafts.
    pub(super) fn accept(self, ticket: u64, of: &str, selected: &str) -> Option<bool> {
        if !self.latest.is(ticket) || of != selected {
            return None;
        }
        let fresh = self.seeded.try_get_value()?.as_deref() != Some(of);
        self.seeded.set_value(Some(of.to_string()));
        Some(fresh)
    }
}

/// The selected agent's detail beside the list: its header bar (the back
/// button in a narrow window, the tabs, the run buttons, the agent's menu)
/// and the tab's body under it.
#[component]
pub fn AgentDetailPage(narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let agents = use_context::<AgentsCtx>();
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
            // The old link's query is remembered as the path's last one by
            // now (or in this same flush): forgotten once that has run, so
            // the Run tab does not lead back to it.
            let here = format!("/agents/{id}");
            request_animation_frame(move || crate::url_state::forget_query(&here));
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
    let error = RwSignal::new(None::<String>);
    let draft = Draft::new();
    // The Definition tab's editor and its inline report live here, not inside
    // the tab: the tab body is re-rendered on every tab click, so a manifest
    // someone is halfway through writing — and the warnings a save just came
    // back with — would be thrown away by a glance at the Run tab. Both are
    // re-seeded by `load`, which is the only thing allowed to overwrite them.
    let manifest = RwSignal::new(String::new());
    // The manifest as stored: the editor is unsaved while it differs.
    let manifest_base = RwSignal::new(String::new());
    let report = RwSignal::new(Vec::<String>::new());
    // The first answer of a selection replaces both drafts; a re-read of the
    // same agent keeps whichever holds edits ([`DetailReads`]).
    let reads = DetailReads::new();
    // Same reason, and it matters more here: the rows of a run in flight and
    // the checkboxes a reviewer has been ticking cannot survive a tab click if
    // they live inside the tab.
    let run = RunState::new();
    // The Runs list is the page's too: its tab shows the count on every tab.
    let runs: RunList = RwSignal::new(None);

    let acts = AgentActs::new();
    let load = move || {
        let want = id.get_untracked();
        if want.is_empty() {
            return;
        }
        let Some(ticket) = reads.begin() else {
            return;
        };
        run.scope.spawn(async move {
            let res = crate::api::get::<AgentDetail>(&format!("/api/agents/{want}")).await;
            // An answer older than a read since, or of an agent the list has
            // moved on from, would put that agent on this one's page.
            let Some(fresh) = reads.accept(ticket, &want, &id.get_untracked()) else {
                return;
            };
            match res {
                Ok(d) => {
                    // A re-read after something changed the agent (a save,
                    // a run that ended, the switch): its list row says it
                    // too. The first read of a selection changes nothing.
                    if !fresh {
                        if let Some(a) = agents {
                            a.reload.run(());
                        }
                    }
                    if fresh {
                        draft.load(&d.fields, &d.config);
                    } else {
                        // Only when the form is something else now: a run
                        // that ended re-reads the agent, and rebuilding the
                        // rows for the same form would take the caret out of
                        // the one being typed in.
                        let same = detail.with_untracked(|p| {
                            p.as_ref()
                                .is_some_and(|p| p.fields == d.fields && p.config == d.config)
                        });
                        if !same {
                            draft.reload(&d.fields, &d.config);
                        }
                    }
                    // The stored manifest is canonicalized on the way
                    // in, so what is on screen after a save is what is
                    // actually stored — unless the editor holds edits
                    // of its own, which a re-read for anything else
                    // (a config save, the service) must not wipe.
                    let clean =
                        manifest.with_untracked(|m| manifest_base.with_untracked(|b| m == b));
                    if fresh || clean {
                        manifest.set(d.manifest.clone());
                    }
                    manifest_base.set(d.manifest.clone());
                    // Set only when it changes: every write of a signal
                    // re-runs what reads it, and the detail is rebuilt on it.
                    if error.with_untracked(Option::is_some) {
                        error.set(None);
                    }
                    detail.set(Some(d));
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
        reads.forget();
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
    // Keyed on the agent too: the page stays mounted while the list's
    // selection moves, and the next batch agent's runs are read afresh.
    let batchy =
        Memo::new(move |_| detail.with(|d| d.as_ref().map(|d| (d.id.clone(), d.kind != "chat"))));
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
        if batchy.with(|b| b.as_ref().is_some_and(|(_, batch)| *batch)) {
            load_runs.run(());
        }
    });

    let reload = Callback::new(move |()| load());

    // The list's search travels with the tabs, as it does with a row
    // ([`crate::pages::agents::list_query`]): a tab click keeps the list as
    // it was filtered.
    let location = use_location();
    let kept = Memo::new(move |_| list_query(&location.search.get()));
    let tabs = Signal::derive(move || {
        let q = kept.get();
        detail.with(|d| {
            let Some(d) = d else { return Vec::new() };
            let id = d.id.as_str();
            let href = |tab: &str| format!("{}{q}", tab_href(id, tab));
            let mut v = vec![NavTab::new("Run", href("run")).exact()];
            if d.kind == "chat" {
                v.push(NavTab::new("Threads", href("runs")).count(d.threads));
            } else {
                let mut t = NavTab::new("Runs", href("runs"));
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
                v.push(NavTab::new("App", href("ui")));
            }
            v.push(NavTab::new("Definition", href("definition")));
            v
        })
    });
    let scrolled = RwSignal::new(false);
    // The fill tabs (a table, the app's frame, the manifest) start under a
    // bar of their own: the header draws its line over them.
    Effect::new(move |_| {
        if tab.get() != "run" {
            scrolled.set(true);
        } else {
            scrolled.set(false);
        }
    });
    let title = Signal::derive(move || {
        detail
            .with(|d| {
                d.as_ref().map(|d| {
                    if d.name.is_empty() {
                        d.id.clone()
                    } else {
                        d.name.clone()
                    }
                })
            })
            .unwrap_or_else(|| id.get())
    });

    // What the detail is rebuilt on: the agent and its run kind (the tabs,
    // the header's buttons and the Run tab's groups are the kind's). Every
    // other change of the agent — a run that ended, a save, the switch —
    // reaches the parts that show it through `shown`, so the manifest being
    // typed, a focused config field and an open dialog stay as they are.
    let shell =
        Memo::new(move |_| detail.with(|d| d.as_ref().map(|d| (d.id.clone(), d.kind.clone()))));

    view! {
        {move || {
            if let Some(e) = error.get() {
                return view! {
                    <DetailHead narrow=narrow back=back title=title scrolled=scrolled/>
                    <div class="detail-scroll mdetail-scroll">
                        <div class="detail-flow">
                            <div class="mdetail-empty">
                                <p>{crate::labels::capitalise(&e)}</p>
                                <p>"It was deleted, or the link names an agent that never was."</p>
                                <button type="button" class="btn" on:click=move |_| back.run(())>
                                    "Back to the list"
                                </button>
                            </div>
                        </div>
                    </div>
                }
                .into_any();
            }
            let Some((_, kind)) = shell.get() else {
                return view! {
                    <DetailHead narrow=narrow back=back title=title scrolled=scrolled/>
                    <div class="detail-scroll mdetail-scroll">
                        <div class="mdetail-empty">"Loading…"</div>
                    </div>
                }
                .into_any();
            };
            // The agent as last read, for the parts that follow it; it keeps
            // the last one while the page swaps to an error.
            let shown: Memo<AgentDetail> = Memo::new(move |prev: Option<&AgentDetail>| {
                detail.get().or_else(|| prev.cloned()).unwrap_or_default()
            });
            // One run context per agent, shared by the header's buttons and
            // the Run tab: when a button is off, and why, is decided in one
            // place.
            let ctx = RunCtx::new(shown, draft, run);
            let more = Signal::derive(move || shown.with(|d| menu_items(d, acts, reload)));
            // A row this build cannot read still opens (container-runtime
            // §4.3): the detail comes back with `error` set instead of a 400,
            // so the Definition editor stays reachable for the very manifest
            // that needs fixing. Said over every tab.
            let row_error = Memo::new(move |_| shown.with(|d| d.error.clone()));
            // What the App tab is built from: its frame is rebuilt only when
            // the service or its dev override changes.
            let app_key = Memo::new(move |_| {
                shown.with(|d| (d.service.clone(), d.dev_url.clone(), d.fields.clone()))
            });
            view! {
                <DetailHead
                    narrow=narrow
                    back=back
                    title=title
                    scrolled=scrolled
                    back_label="Back to the agents"
                    tabs=move || view! { <SubNav tabs=tabs/> }
                    actions=move || {
                        let kind = kind.clone();
                        // Rebuilt when the manifest's shape says another
                        // set of buttons (a save that adds a classify).
                        move || {
                            ctx.shape.track();
                            view! { <RunActions kind=kind.clone() ctx=ctx/> }
                        }
                    }
                    more=more
                />
                {move || {
                    row_error
                        .get()
                        .map(|e| {
                            view! {
                                <div class="notice err adetail-notice">
                                    <b>{e}</b>
                                    <span class="detail">
                                        "The Definition tab has the stored text as it is; nothing else on "
                                        "this page can be trusted until it parses."
                                    </span>
                                </div>
                            }
                        })
                }}
                // Every tab but Run's setup fills the body: each brings its
                // own one scroller (a table, the app's frame, the manifest).
                {move || match tab.get() {
                    "runs" => {
                        view! {
                            <div class="adetail-body">
                                <RunsTab
                                    d=shown.get_untracked()
                                    run=run
                                    runs=runs
                                    reload_runs=load_runs
                                />
                            </div>
                        }
                            .into_any()
                    }
                    "ui" => {
                        view! {
                            <div class="adetail-body">
                                {move || {
                                    app_key.track();
                                    view! { <AppTab d=shown.get_untracked() reload=reload/> }
                                }}
                            </div>
                        }
                            .into_any()
                    }
                    "definition" => {
                        view! {
                            <div class="adetail-body">
                                <DefinitionTab
                                    d=shown
                                    text=manifest
                                    base=manifest_base
                                    lines=report
                                    reload=reload
                                />
                            </div>
                        }
                            .into_any()
                    }
                    _ => {
                        view! {
                            <RunTab
                                d=shown
                                ctx=ctx
                                reload=reload
                                runs=runs
                                acts=acts
                                scrolled=scrolled
                            />
                        }
                            .into_any()
                    }
                }}
                <AgentDialogs d=shown acts=acts reload=reload/>
            }
            .into_any()
        }}
    }
}
