use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{
    AgentBatchShape, AgentConfigSaved, AgentDecide, AgentDetail, AgentField, AgentRunCancelled,
    AgentRuntime, AgentWarning,
};
use serde_json::{json, Value};

use super::*;
use crate::pages::agents::short_ts;
use crate::widgets::nav_split::{track_scroll, SaveKey};
use crate::widgets::schema_form::{Draft, SchemaForm};
use crate::widgets::{
    use_toasts, ActionRow, Explain, Ico, Icon, InfoRow, PrefGroup, SaveToast, Side, SplitPane,
    StackRow, Switch,
};

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// Everything the Run tab's pieces share about one agent and its run surface,
/// as one `Copy` bundle: the run column, the config form and the review all
/// start, re-run and apply through it, so when a button is off — and why — is
/// decided in one place.
#[derive(Clone, Copy)]
pub(super) struct RunCtx {
    pub(super) agent_id: StoredValue<String>,
    pub(super) fields: Signal<Vec<AgentField>>,
    /// The config form, sent with every start: a run uses the values as they
    /// stand at the click, the same way a thread does.
    pub(super) draft: Draft,
    pub(super) run: RunState,
    pub(super) toasts: crate::widgets::Toasts,
    /// What this manifest can actually do, from the server: a button for a
    /// stage the agent does not declare would be a button that fails. The
    /// shape a *run* came back with lives in `run.shape` and is what the table
    /// is drawn from, so reopening an old run cannot be confused by an edit to
    /// the manifest since.
    pub(super) shape: Memo<AgentBatchShape>,
    /// What a `container` agent runs under. `None` for a `batch` agent.
    pub(super) runtime: Memo<Option<AgentRuntime>>,
    pub(super) budget: Memo<lmgw_api_types::AgentBudget>,
    /// The gateway's display currency, for the run's cost line.
    pub(super) currency: Memo<String>,
    /// Why nothing may start right now — the agent is off, its tools do not
    /// resolve, a warning blocks it (§4.3), or the form is missing a required
    /// value — or `None`. Reactive, because the form half of it is.
    pub(super) blocked_by: Signal<Option<String>>,
    /// Why a classify (Dry run, Re-run attention) may not start: what
    /// `blocked_by` says, or an output that can decide through a question
    /// set with neither the model nor a set in the form (labs-store design
    /// §4). List only needs neither.
    pub(super) classify_blocked_by: Signal<Option<String>>,
    /// The config form's own problem: a number that does not parse (the form
    /// is where to say it).
    pub(super) problem: RwSignal<Option<String>>,
    /// A save the server refused (it names the field): the save toast says it.
    save_error: RwSignal<Option<String>>,
    saving: RwSignal<bool>,
}

impl RunCtx {
    /// Made once per agent, from the agent as last read: each part follows
    /// it on its own, so a re-read (a run that ended, a save) moves what it
    /// changed and leaves the rest — the form's rows above all — as it is.
    pub(super) fn new(d: Memo<AgentDetail>, draft: Draft, run: RunState) -> Self {
        let fields = Memo::new(move |_| d.with(|d| d.fields.clone()));
        let fields: Signal<Vec<AgentField>> = fields.into();
        // Tracked, so filling the field in un-blocks the buttons as you type
        // rather than at the next save.
        let gap = Signal::derive(move || config_gap(&unfilled_required(&fields.get(), draft)));
        let shape = Memo::new(move |_| d.with(|d| d.batch.clone().unwrap_or_default()));
        let classify_gap = Signal::derive(move || {
            shape.with(|s| {
                s.decide
                    .as_ref()
                    .and_then(|d| unfilled_model_and_set(&fields.get(), draft, d))
            })
        });
        // What may stop a start, apart from the form: the switch, the tools,
        // and §4.3's warnings, where `blocks_start` is what turns one of them
        // into a disabled button rather than a line of text.
        let gate = Memo::new(move |_| {
            d.with(|d| {
                (
                    d.enabled,
                    d.requires_ok,
                    d.kind == "chat",
                    d.warnings
                        .iter()
                        .find(|w| w.blocks_start)
                        .map(|w| w.message.clone()),
                )
            })
        });
        let blocked_by = Signal::derive(move || {
            let (enabled, requires_ok, chat, blocking) = gate.get();
            if !enabled {
                Some("This agent is disabled".to_string())
            } else if chat {
                // A thread with a missing tool server still opens and says
                // so; only an unfilled form stops it (`agent_open_chat`
                // refuses one, naming the field).
                gap.get()
            } else if !requires_ok {
                Some("This agent's tools do not resolve on this gateway".to_string())
            } else if let Some(w) = blocking {
                Some(w)
            } else {
                gap.get()
            }
        });
        let classify_blocked_by =
            Signal::derive(move || blocked_by.get().or_else(|| classify_gap.get()));
        Self {
            agent_id: StoredValue::new(d.with_untracked(|d| d.id.clone())),
            fields,
            draft,
            run,
            toasts: use_toasts(),
            shape,
            runtime: Memo::new(move |_| d.with(|d| d.runtime.clone())),
            budget: Memo::new(move |_| d.with(|d| d.budget.clone())),
            currency: Memo::new(move |_| d.with(|d| d.currency.clone())),
            blocked_by,
            classify_blocked_by,
            problem: RwSignal::new(None),
            save_error: RwSignal::new(None),
            saving: RwSignal::new(false),
        }
    }

    pub(super) fn blocked(&self) -> bool {
        self.blocked_by.with(Option::is_some)
    }

    pub(super) fn classify_blocked(&self) -> bool {
        self.classify_blocked_by.with(Option::is_some)
    }

    /// The form at the moment of *this* click, so a re-run and an apply
    /// pressed minutes apart each carry what was on screen then. A number the
    /// form cannot parse is the one thing refused here, for want of a value to
    /// send.
    fn form_values(&self) -> Option<serde_json::Map<String, Value>> {
        match self.draft.patch(&self.fields.get_untracked()) {
            Ok(p) => Some(p),
            Err(e) => {
                self.toasts.err(e);
                None
            }
        }
    }

    pub(super) fn start(&self, phase: &'static str) {
        let Some(values) = self.form_values() else {
            return;
        };
        let id = self.agent_id.get_value();
        self.run.start(
            id.clone(),
            json!({ "id": id, "phase": phase, "values": values }),
            self.toasts,
        );
    }

    pub(super) fn rerun(&self) {
        let Some(job_id) = self.run.job_id() else {
            return;
        };
        let Some(values) = self.form_values() else {
            return;
        };
        let id = self.agent_id.get_value();
        self.run.start(
            id.clone(),
            json!({ "id": id, "phase": "rerun", "base_job": job_id, "values": values }),
            self.toasts,
        );
    }

    pub(super) fn apply(&self) {
        let run = self.run;
        run.confirm_apply.set(false);
        let out = run.unchecked.get_untracked();
        let overrides = run.overrides.get_untracked();
        let rows: Vec<Value> = run
            .rows
            .get_untracked()
            .iter()
            .filter(|r| !out.contains(&r.id))
            .map(|r| r.for_apply(&overrides))
            .collect();
        let Some(values) = self.form_values() else {
            return;
        };
        let id = self.agent_id.get_value();
        run.start(
            id.clone(),
            json!({ "id": id, "phase": "apply", "rows": rows, "values": values }),
            self.toasts,
        );
    }

    pub(super) fn cancel(&self) {
        let id = self.agent_id.get_value();
        let toasts = self.toasts;
        spawn_local(async move {
            match crate::api::post::<AgentRunCancelled, _>(
                "/api/op/agent_run_cancel",
                &json!({ "id": id }),
            )
            .await
            {
                Ok(_) => toasts.ok("cancel requested"),
                Err(e) => toasts.err(e.to_string()),
            }
        });
    }

    fn save_config(&self, reload: Callback<()>) {
        if self.saving.get_untracked() {
            return;
        }
        let fields = self.fields.get_untracked();
        let patch = match self.draft.patch(&fields) {
            Ok(p) => p,
            Err(e) => {
                self.problem.set(Some(e));
                return;
            }
        };
        // Ticked "clear" boxes travel as their own list: an empty control means
        // "keep the stored secret", so forgetting one has to be said out loud.
        let clear = self.draft.clear_list(&fields);
        let (problem, save_error, saving, toasts, draft) = (
            self.problem,
            self.save_error,
            self.saving,
            self.toasts,
            self.draft,
        );
        let sent = draft.snapshot();
        let id = self.agent_id.get_value();
        problem.set(None);
        save_error.set(None);
        saving.set(true);
        spawn_local(async move {
            let res = crate::api::post::<AgentConfigSaved, _>(
                "/api/op/agent_config_set",
                &json!({ "id": id, "values": patch, "clear": clear }),
            )
            .await;
            saving.set(false);
            match res {
                Ok(_) => {
                    toasts.ok("config saved");
                    // The form as sent is what is stored now (a no-op on a
                    // page that is gone), then the re-read follows it.
                    draft.mark_saved(sent);
                    // `try_`: a no-op once the page is gone (review code:C4).
                    reload.try_run(());
                }
                // The server validates against the schema and names the field;
                // the save toast keeps it until the next save.
                Err(e) => save_error.set(Some(e.to_string())),
            }
        });
    }
}

/// The Run tab. With no run on screen it is the agent's overview as a
/// preferences page ([`RunSetup`]); with a run open (live, just finished, or
/// reopened from Runs) the review takes the body: the run group and the
/// config fold into a panel on the right, and the table, its filters and
/// Apply fill the rest. The config's save toast and Ctrl+S are there in both.
#[component]
pub(super) fn RunTab(
    /// The agent as last read; the tab is made once per agent and its
    /// groups follow it.
    d: Memo<AgentDetail>,
    ctx: RunCtx,
    reload: Callback<()>,
    runs: RunList,
    acts: AgentActs,
    scrolled: RwSignal<bool>,
) -> impl IntoView {
    let run = ctx.run;
    let batchy = d.with_untracked(|d| d.kind != "chat");
    let reviewing = Memo::new(move |_| batchy && run.summary.with(Option::is_some));
    let draft = ctx.draft;
    let unsaved = Memo::new(move |_| draft.changed().len());
    // Like the Definition tab's: a form with nothing changed is not sent, or
    // the schema's defaults it shows would be stored as chosen values.
    SaveKey::install(Callback::new(move |()| {
        if unsaved.get_untracked() > 0 {
            ctx.save_config(reload);
        }
    }));
    // The review is drawn from the shape the manifest declares: a manifest
    // saved with another one rebuilds it.
    let review = move || {
        ctx.shape.track();
        view! { <ReviewPane ctx=ctx/> }
    };
    view! {
        <Show
            when=move || reviewing.get()
            fallback=move || {
                view! {
                    <RunSetup d=d ctx=ctx reload=reload runs=runs acts=acts scrolled=scrolled/>
                }
            }
        >
            <div class="adetail-body">
                <SplitPane
                    side=Side::Right
                    persist="agents.review"
                    label="Config & run"
                    default_open=false
                    width=(300, 30, 420)
                    auto_collapse_below=640
                    class="review-dock"
                    side_view=move || {
                        view! {
                            <div class="run-side-panel">
                                {run_group(d, ctx, runs)}
                                <ConfigGroup ctx=ctx/>
                            </div>
                        }
                    }
                >
                    {review}
                </SplitPane>
            </div>
        </Show>
        <SaveToast
            dirty_count=Signal::derive(move || unsaved.get())
            saving=ctx.saving
            error=ctx.save_error
            on_save=Callback::new(move |()| ctx.save_config(reload))
            on_discard=Callback::new(move |()| {
                ctx.save_error.set(None);
                draft.discard();
            })
            save_label="Save config"
        />
    }
}

/// The run group, rebuilt only when what it shows of the agent changes (its
/// kind, its threads, what the manifest can classify), not on every re-read.
fn run_group(d: Memo<AgentDetail>, ctx: RunCtx, runs: RunList) -> impl IntoView {
    let key = Memo::new(move |_| {
        let has_classify = ctx.shape.with(|s| s.has_classify);
        d.with(|d| (d.id.clone(), d.kind.clone(), d.threads, has_classify))
    });
    move || {
        key.track();
        view! { <RunGroup d=d.get_untracked() ctx=ctx runs=runs/> }
    }
}

/// No run on screen: the hero, what stands in the way of a run, the run
/// kind's group, the config form as rows, the runtime and the package, the
/// agent's own switch and the Delete row, in two columns over 1000 px.
///
/// Made once per agent: each group follows the part of the agent it shows,
/// so a re-read leaves the config form's rows (and the field being typed
/// in) alone.
#[component]
fn RunSetup(
    d: Memo<AgentDetail>,
    ctx: RunCtx,
    reload: Callback<()>,
    runs: RunList,
    acts: AgentActs,
    scrolled: RwSignal<bool>,
) -> impl IntoView {
    let attention =
        Memo::new(move |_| d.with(|d| (d.id.clone(), d.requires.clone(), d.warnings.clone())));
    let package =
        Memo::new(move |_| d.with(|d| (d.id.clone(), d.runtime.clone(), d.provenance.clone())));
    let agent = Memo::new(move |_| {
        d.with(|d| {
            (
                d.id.clone(),
                d.enabled,
                d.source.clone(),
                d.version.clone(),
                d.updated_at.clone(),
                d.token.scope_note.clone(),
            )
        })
    });
    view! {
        <div
            class="detail-scroll mdetail-scroll run-setup"
            tabindex="-1"
            on:scroll=move |ev| track_scroll(&ev, scrolled)
        >
            <div class="detail-flow">
                <Hero d=d run=ctx.run/>
                {move || {
                    attention.track();
                    view! { <Attention d=d.get_untracked()/> }
                }}
                {run_group(d, ctx, runs)}
                <ConfigGroup ctx=ctx/>
                // Whatever the run kind is: the image and its limits belong
                // to a container agent, but the *package* half belongs to any
                // row installed from an image — a `chat` agent can arrive that
                // way too, and would otherwise have nowhere to show where it
                // came from.
                {move || {
                    let (id, runtime, provenance) = package.get();
                    view! {
                        <RuntimeBlock id=id runtime=runtime provenance=provenance reload=reload/>
                    }
                }}
                {move || {
                    agent.track();
                    view! { <AgentGroup d=d.get_untracked() acts=acts reload=reload/> }
                }}
                <PrefGroup>
                    <ActionRow
                        title="Delete agent…"
                        sub="Removes it from the catalog; its chat threads are kept"
                        danger=true
                        on_click=Callback::new(move |()| acts.del_open.set(true))
                    />
                </PrefGroup>
            </div>
        </div>
    }
}

/// The config form (§2.6) as a boxed group of rows.
#[component]
fn ConfigGroup(ctx: RunCtx) -> impl IntoView {
    let problem = ctx.problem;
    let decide = Memo::new(move |_| ctx.shape.with(|s| s.decide.clone()));
    view! {
        <section class="pref-group config-group">
            <div class="pref-head">
                <h2>"Config"</h2>
                <span class="count">{move || ctx.fields.with(Vec::len)}</span>
            </div>
            // Said out loud, because the two are deliberately different acts
            // and nothing else on the page would tell you.
            <p class="pref-desc">
                "A start uses the form as it stands; Save config sets what the next one opens with."
            </p>
            <div class="boxed">
                // Rebuilt only for another question-set output: the rows
                // follow the fields themselves.
                {move || {
                    let decide = decide.get();
                    view! { <SchemaForm fields=ctx.fields draft=ctx.draft decide=decide/> }
                }}
                {move || problem.get().map(|p| view! { <div class="prow problem-row">{p}</div> })}
            </div>
        </section>
    }
}

/// The status chip's class for a run status.
pub(super) fn status_chip(status: &str) -> &'static str {
    match status {
        "done" => "chip ok",
        "failed" => "chip err",
        "running" | "queued" => "chip info",
        _ => "chip off",
    }
}

/// What a run of this kind is and does, as the run group's heading, its
/// sentence and the fold under it.
fn run_words(kind: &str) -> (&'static str, &'static str, &'static str) {
    match kind {
        "chat" => (
            "Chat",
            "Opening creates an ordinary Chat thread seeded from this manifest.",
            "The model in Config, the system prompt and the tools the agent attaches go with \
             it. It then behaves like any other thread, and lists under Threads here.",
        ),
        "container" => (
            "Container run",
            "A run starts the agent's image and writes nothing: the review is the gate.",
            "The image reports rows on stdout. Check the rows you want, fix what needs fixing, \
             then apply, which runs the same image again with LMGW_PHASE=apply.",
        ),
        _ => (
            "Batch run",
            "A run lists, fetches and classifies, and writes nothing: the review is the gate.",
            "Check the rows you want, fix what the model got wrong, then apply.",
        ),
    }
}

/// The run kind's group: what stands in the way of a start, the last run
/// (one click from its review) or the threads, and what a start mounts. The
/// start buttons are the header's ([`RunActions`]).
#[component]
fn RunGroup(d: AgentDetail, ctx: RunCtx, runs: RunList) -> impl IntoView {
    let run = ctx.run;
    let is_chat = d.kind == "chat";
    let has_classify = ctx.shape.with_untracked(|s| s.has_classify);
    let (head, summary, body) = run_words(&d.kind);
    let threads = d.threads;
    let runs_href = tab_href(&d.id, "runs");
    // The last run, one click from its review — only while none is on screen.
    let last = Signal::derive(move || {
        runs.with(|r| {
            r.as_ref()
                .and_then(|r| r.as_ref().ok())
                .and_then(|l| l.first().cloned())
        })
    });
    let loaded = Signal::derive(move || runs.with(|r| r.as_ref().is_some_and(Result::is_ok)));
    let show_last =
        move || !is_chat && run.summary.with(Option::is_none) && last.with(Option::is_some);
    let never = move || !is_chat && loaded.get() && last.with(Option::is_none);
    view! {
        <section class="pref-group run-group">
            <div class="pref-head">
                <h2>{head}</h2>
            </div>
            <Explain summary=summary persist="agents.explain.run">
                {body}
            </Explain>
            <div class="boxed">
                // Named, because the form is right there: most of these are a
                // two-second fix, not a fault. A batch agent that classifies
                // says what keeps its classify off too, which is everything
                // that keeps List off and more.
                {move || {
                    let why = if has_classify {
                        ctx.classify_blocked_by.get()
                    } else {
                        ctx.blocked_by.get()
                    };
                    match why {
                        Some(w) => view! { <div class="prow notice-row warn run-why">{w}</div> }.into_any(),
                        None => view! {
                            <div class="prow notice-row run-ready">
                                "Ready: the header's buttons start it with the form as it stands."
                            </div>
                        }
                        .into_any(),
                    }
                }}
                {is_chat
                    .then(|| {
                        view! {
                            <a class="prow action" href=runs_href.clone()>
                                <div class="prow-text">
                                    <span class="prow-title">"Threads"</span>
                                    <span class="prow-sub">"Opened from this agent, on the Threads tab"</span>
                                </div>
                                <span class="prow-value">{threads}</span>
                                <Icon ico=Ico::Right class="prow-chevron"/>
                            </a>
                        }
                    })}
                <Show when=show_last>
                    {move || {
                        last.get()
                            .map(|r| {
                                let job_id = r.job_id;
                                let when = short_ts(r.finished_at.as_deref().unwrap_or(&r.created_at));
                                view! {
                                    <div class="prow run-last">
                                        <div class="prow-text">
                                            <span class="prow-title">"Last run"</span>
                                            <span class="prow-sub mono-sm">
                                                {format!("#{job_id} · {} · {when}", r.phase)}
                                            </span>
                                        </div>
                                        <div class="prow-end">
                                            <span class=status_chip(&r.status)>
                                                <span class="dot"></span>
                                                {r.status.clone()}
                                            </span>
                                            <button
                                                class="btn flat sm link-btn"
                                                title="show this run's rows here"
                                                on:click=move |_| {
                                                    run.load(job_id);
                                                    run.pinned.set(Some(job_id));
                                                }
                                            >
                                                "Open"
                                            </button>
                                        </div>
                                    </div>
                                }
                            })
                    }}
                </Show>
                <Show when=never>
                    <div class="prow notice-row">"Never run yet."</div>
                </Show>
                {(!is_chat).then(|| view! { <MountSummary fields=ctx.fields draft=ctx.draft/> })}
            </div>
        </section>
    }
}

/// What stands in the way of this agent, before anything else: the tool
/// gaps (with the way to the MCP page) and §4.3's warnings, a blocking one in
/// red. A manifest this build cannot read is said over every tab instead
/// (the page's notice).
#[component]
fn Attention(d: AgentDetail) -> impl IntoView {
    let gaps: Vec<String> = d
        .requires
        .iter()
        .filter(|r| !r.registered || !r.missing_tools.is_empty())
        .map(|r| {
            if !r.registered {
                format!(
                    "'{}' is not a registered MCP server on this gateway.{}",
                    r.label,
                    r.install
                        .as_ref()
                        .map(|i| format!(" The manifest suggests: {} {}.", i.kind, i.reference))
                        .unwrap_or_default()
                )
            } else {
                format!(
                    "'{}' does not currently list: {}",
                    r.label,
                    r.missing_tools.join(", ")
                )
            }
        })
        .collect();
    if gaps.is_empty() && d.warnings.is_empty() {
        return None;
    }
    let definition = tab_href(&d.id, "definition");
    Some(view! {
        <PrefGroup title="Needs attention">
            {gaps
                .into_iter()
                .map(|g| {
                    view! {
                        <div class="prow notice-row warn">
                            <div class="prow-text">
                                <b>{g}</b>
                                <span class="prow-sub">
                                    "Register it on " <a href="/mcp-servers">"MCP servers"</a>
                                    "; the agent is installed and waiting."
                                </span>
                            </div>
                        </div>
                    }
                })
                .collect_view()}
            {d.warnings
                .iter()
                .map(|w: &AgentWarning| {
                    let cls = if w.blocks_start { "prow problem-row" } else { "prow notice-row warn" };
                    // §5.1's way out lives on the Definition tab, with the rest
                    // of what replaces the manifest; the row points there.
                    let reset = (w.code == "builtin_update_available").then(|| {
                        let href = definition.clone();
                        view! {
                            <span class="prow-sub">
                                "Reset to shipped is in the header's menu; the shipped text is under "
                                <a href=href>"Definition"</a> "."
                            </span>
                        }
                    });
                    view! {
                        <div class=cls title=format!("Code: {}", w.code)>
                            <div class="prow-text">
                                <b>{w.message.clone()}</b>
                                {reset}
                            </div>
                        </div>
                    }
                })
                .collect_view()}
        </PrefGroup>
    })
}

/// The agent itself: its switch (which applies at once), where it came
/// from, when it changed, and what its own token may reach.
#[component]
fn AgentGroup(d: AgentDetail, acts: AgentActs, reload: Callback<()>) -> impl IntoView {
    let on = RwSignal::new(d.enabled);
    let id = StoredValue::new(d.id.clone());
    Effect::new(move |prev: Option<bool>| {
        let v = on.get();
        if prev.is_some_and(|p| p != v) {
            acts.set_enabled(id.get_value(), v, reload);
        }
        v
    });
    let source = match d.source.as_str() {
        "builtin" => "Shipped with lmgw",
        "imported" => "Imported",
        "authored" => "Written here",
        _ => "Unknown",
    };
    let version = (!d.version.is_empty()).then(|| format!("v{}", d.version));
    let scope_models = d.token.scope_models.clone();
    view! {
        <PrefGroup title="Agent">
            <div class="prow action" on:click=move |_| {
                if !acts.busy.get_untracked() {
                    on.update(|v| *v = !*v);
                }
            }>
                <div class="prow-text">
                    <span class="prow-line">
                        <span class="prow-title">"Enabled"</span>
                        <span class="prow-tag">"applies now"</span>
                    </span>
                    <span class="prow-sub">"A disabled agent stays in the catalog and refuses to run"</span>
                </div>
                <Switch value=on label="Enabled" disabled=Signal::derive(move || acts.busy.get())/>
            </div>
            <InfoRow title="Source" value=source/>
            {version.map(|v| view! { <InfoRow title="Version" value=v/> })}
            <InfoRow title="Updated" value=short_ts(&d.updated_at) mono=true/>
            // Which models this agent's bearer can reach (par:PAR-7): it
            // changes with every saved model field.
            {if scope_models.is_empty() {
                view! { <InfoRow title="Its token reaches" value="Any model"/> }.into_any()
            } else {
                view! {
                    <StackRow title="Its token reaches">
                        <div class="chips">
                            {scope_models
                                .into_iter()
                                .map(|m| view! { <span class="chip mono-sm">{m}</span> })
                                .collect_view()}
                        </div>
                    </StackRow>
                }
                    .into_any()
            }}
        </PrefGroup>
    }
}

/// The required config fields the **form** has nothing in, by the label it
/// shows them under.
///
/// The form, not the stored config, because the form is what a start uses:
/// `agent_open_chat` and `agent_run` both take the values as they stand at the
/// click and apply them over what is saved, for that one thread or run. Save
/// config sets the defaults a start falls back to, and the two are deliberately
/// separate — picking a different model for one conversation must not rewrite
/// the agent.
///
/// Asked here so the buttons can refuse before the click: the same question
/// `manifest::validate_values` asks server-side, which would otherwise answer
/// with a toast naming a field.
fn unfilled_required(fields: &[AgentField], draft: Draft) -> Vec<String> {
    fields
        .iter()
        .filter(|f| f.required && f.default.is_none() && !draft.has_value(f))
        .map(label_of)
        .collect()
}

/// The config fields' label: their title, else their name.
fn label_of(f: &AgentField) -> String {
    if f.title.is_empty() {
        f.name.clone()
    } else {
        f.title.clone()
    }
}

/// Why a classify cannot start for want of a model and a question set, or
/// `None`: an output that decides through a set needs one of the two, and
/// the model is a config field the form can hold (`decide.model_field`).
/// The form, not the stored config, as [`unfilled_required`] asks it.
pub(super) fn unfilled_model_and_set(
    fields: &[AgentField],
    draft: Draft,
    decide: &AgentDecide,
) -> Option<String> {
    let model = fields
        .iter()
        .find(|f| decide.model_field.as_deref() == Some(f.name.as_str()))?;
    let set = fields.iter().find(|f| f.name == decide.field)?;
    let filled = |f: &AgentField| f.default.is_some() || draft.has_value(f);
    if filled(model) || filled(set) {
        return None;
    }
    Some(model_or_set_gap(&label_of(model), &label_of(set)))
}

/// What Classify says when neither the model nor a question set is in the
/// form, named as the form shows them.
pub(super) fn model_or_set_gap(model: &str, set: &str) -> String {
    format!("Set {model} or {set} in Config below — a classify needs one of them")
}

/// What the Start and Open buttons say when they are off, or `None` when the
/// form is ready. Split out from [`unfilled_required`] so the wording has a
/// test without a reactive runtime behind it.
pub(super) fn config_gap(unfilled: &[String]) -> Option<String> {
    if unfilled.is_empty() {
        return None;
    }
    // Named, because the form is right above: this is a two-second fix, not a
    // fault.
    Some(format!(
        "Set {} in Config below — this agent ships no default for it",
        unfilled.join(" and ")
    ))
}
