use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;
use lmgw_api_types::{AgentBatchShape, AgentDetail, AgentField, AgentRuntime, AgentWarning};
use serde_json::{json, Value};

use super::*;
use crate::pages::agents::short_ts;
use crate::widgets::schema_form::{Draft, SchemaForm};
use crate::widgets::{use_toasts, Explain, Side, SplitPane};

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// Everything the Run tab's pieces share about one agent and its run surface,
/// as one `Copy` bundle: the run column, the config form and the review all
/// start, re-run and apply through it, so when a button is off — and why — is
/// decided in one place.
#[derive(Clone, Copy)]
pub(super) struct RunCtx {
    agent_id: StoredValue<String>,
    fields: Signal<Vec<AgentField>>,
    /// The config form, sent with every start: a run uses the values as they
    /// stand at the click, the same way a thread does.
    draft: Draft,
    pub(super) run: RunState,
    toasts: crate::widgets::Toasts,
    /// What this manifest can actually do, from the server: a button for a
    /// stage the agent does not declare would be a button that fails. The
    /// shape a *run* came back with lives in `run.shape` and is what the table
    /// is drawn from, so reopening an old run cannot be confused by an edit to
    /// the manifest since.
    pub(super) shape: StoredValue<AgentBatchShape>,
    /// What a `container` agent runs under. `None` for a `batch` agent.
    pub(super) runtime: StoredValue<Option<AgentRuntime>>,
    pub(super) budget: StoredValue<lmgw_api_types::AgentBudget>,
    /// The gateway's display currency, for the run's cost line.
    pub(super) currency: StoredValue<String>,
    /// Why nothing may start right now — the agent is off, its tools do not
    /// resolve, a warning blocks it (§4.3), or the form is missing a required
    /// value — or `None`. Reactive, because the form half of it is.
    pub(super) blocked_by: Signal<Option<String>>,
    /// The config form's own problem: a number that does not parse, a save
    /// the server refused (it names the field; the form is where to say it).
    problem: RwSignal<Option<String>>,
    saving: RwSignal<bool>,
}

impl RunCtx {
    fn new(d: &AgentDetail, draft: Draft, run: RunState) -> Self {
        let fields = StoredValue::new(d.fields.clone());
        let fields: Signal<Vec<AgentField>> = Signal::derive(move || fields.get_value());
        // Tracked, so filling the field in un-blocks the buttons as you type
        // rather than at the next save.
        let gap = Signal::derive(move || config_gap(&unfilled_required(&fields.get(), draft)));
        let enabled = d.enabled;
        let requires_ok = d.requires_ok;
        let chat = d.kind == "chat";
        // §4.3's warnings: `blocks_start` is what turns one of these into a
        // disabled button rather than a line of text.
        let blocking = StoredValue::new(
            d.warnings
                .iter()
                .find(|w| w.blocks_start)
                .map(|w| w.message.clone()),
        );
        let blocked_by = Signal::derive(move || {
            if !enabled {
                Some("this agent is disabled".to_string())
            } else if chat {
                // A thread with a missing tool server still opens and says
                // so; only an unfilled form stops it (`agent_open_chat`
                // refuses one, naming the field).
                gap.get()
            } else if !requires_ok {
                Some("this agent's tools do not resolve on this gateway".to_string())
            } else if let Some(w) = blocking.get_value() {
                Some(w)
            } else {
                gap.get()
            }
        });
        Self {
            agent_id: StoredValue::new(d.id.clone()),
            fields,
            draft,
            run,
            toasts: use_toasts(),
            shape: StoredValue::new(d.batch.clone().unwrap_or_default()),
            runtime: StoredValue::new(d.runtime.clone()),
            budget: StoredValue::new(d.budget.clone()),
            currency: StoredValue::new(d.currency.clone()),
            blocked_by,
            problem: RwSignal::new(None),
            saving: RwSignal::new(false),
        }
    }

    pub(super) fn blocked(&self) -> bool {
        self.blocked_by.with(Option::is_some)
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

    fn start(&self, phase: &'static str) {
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
            match crate::api::post::<Value, _>("/api/op/agent_run_cancel", &json!({ "id": id }))
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
        let (problem, saving, toasts, draft) = (self.problem, self.saving, self.toasts, self.draft);
        let sent = draft.snapshot();
        let id = self.agent_id.get_value();
        problem.set(None);
        saving.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
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
                // showing it at the form beats a toast that scrolls away.
                Err(e) => problem.set(Some(e.to_string())),
            }
        });
    }
}

/// The Run tab. With no run on screen it is the config form beside the run
/// column — Start, what the start will mount, what stands in its way, the
/// runtime folded to a line — which stays in view while the form scrolls.
/// With a run open (live, just finished, or reopened from Runs) the review
/// takes the page: the config and the run column fold into a panel on the
/// right, and the table, its filters and Apply fill the rest.
#[component]
pub(super) fn RunTab(
    d: AgentDetail,
    draft: Draft,
    reload: Callback<()>,
    run: RunState,
    runs: RunList,
) -> impl IntoView {
    let ctx = RunCtx::new(&d, draft, run);
    let batchy = d.kind != "chat";
    let reviewing = Memo::new(move |_| batchy && run.summary.with(Option::is_some));
    let d = StoredValue::new(d);
    view! {
        <Show
            when=move || reviewing.get()
            fallback=move || view! { <RunSetup d=d.get_value() ctx=ctx reload=reload runs=runs/> }
        >
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
                            <RunColumn d=d.get_value() ctx=ctx reload=reload runs=runs/>
                            <ConfigCard ctx=ctx reload=reload/>
                        </div>
                    }
                }
            >
                <ReviewPane ctx=ctx/>
            </SplitPane>
        </Show>
    }
}

#[component]
fn RunSetup(d: AgentDetail, ctx: RunCtx, reload: Callback<()>, runs: RunList) -> impl IntoView {
    view! {
        <div class="fill-pane run-setup">
            <div class="run-grid">
                <div class="run-main">
                    <ConfigCard ctx=ctx reload=reload/>
                </div>
                <aside class="run-side">
                    <RunColumn d=d ctx=ctx reload=reload runs=runs/>
                </aside>
            </div>
        </div>
    }
}

#[component]
fn ConfigCard(ctx: RunCtx, reload: Callback<()>) -> impl IntoView {
    let n = ctx.fields.with_untracked(Vec::len);
    let saving = ctx.saving;
    let problem = ctx.problem;
    let draft = ctx.draft;
    let unsaved = Memo::new(move |_| draft.changed().len());
    view! {
        <section class="card config-card">
            <div class="card-head">
                <span class="mini-head">"Config"</span>
                <span class="count">{n}</span>
                <span class="spacer"></span>
                <Show when=move || unsaved.get() != 0>
                    <span
                        class="count attn"
                        title="Fields that differ from the saved config; a start uses them as they are"
                    >
                        {move || format!("{} unsaved", unsaved.get())}
                    </span>
                    <button
                        class="link-btn"
                        title="Put the saved values back in the form"
                        on:click=move |_| draft.discard()
                    >
                        "Discard"
                    </button>
                </Show>
                <button
                    class="btn sm"
                    title="Keep these values as what the next start opens with"
                    disabled=move || saving.get()
                    on:click=move |_| ctx.save_config(reload)
                >
                    {move || if saving.get() { "Saving…" } else { "Save config" }}
                </button>
            </div>
            <SchemaForm fields=ctx.fields draft=ctx.draft/>
            {move || problem.get().map(|p| view! { <div class="notice err">{p}</div> })}
            // Said out loud, because the two are deliberately different acts
            // and nothing else on the page would tell you.
            <p class="dim mini-note">
                "A start uses the form as it stands; Save config sets what the next one opens with."
            </p>
        </section>
    }
}

/// The status chip's class for a run status.
pub(super) fn status_chip(status: &str) -> &'static str {
    match status {
        "done" => "chip ok",
        "failed" => "chip err",
        "running" | "queued" => "chip live",
        _ => "chip off",
    }
}

/// The run column: the start, what it will mount, what stands in its way,
/// and the runtime folded to one line.
#[component]
fn RunColumn(d: AgentDetail, ctx: RunCtx, reload: Callback<()>, runs: RunList) -> impl IntoView {
    let run = ctx.run;
    let toasts = ctx.toasts;
    let is_chat = d.kind == "chat";
    let is_container = d.kind == "container";
    let has_classify = ctx.shape.with_value(|s| s.has_classify);
    // `StoredValue`, not a local clone: these handlers live inside `<Show>`
    // children, which are re-rendered, so a handler that moved a cloned
    // navigator out of its environment would be `FnOnce` and take the child
    // tree's `Fn` bound with it.
    let navigate = StoredValue::new(use_navigate());
    let busy = RwSignal::new(false);
    let live = Signal::derive(move || run.summary.with(|s| s.as_ref().is_some_and(is_live)));
    let off = Signal::derive(move || ctx.blocked() || run.busy.get() || live.get());
    let why = move || ctx.blocked_by.get().unwrap_or_default();

    let open_chat = move |_| {
        if busy.get_untracked() {
            return;
        }
        // The form as it stands, not what is stored: selecting a model and
        // opening a thread with it is one gesture, and the thread is seeded
        // from what the person is looking at. Save config is a separate act
        // that sets the defaults this falls back to.
        let values = match ctx.draft.patch(&ctx.fields.get_untracked()) {
            Ok(p) => p,
            Err(e) => {
                ctx.problem.set(Some(e));
                return;
            }
        };
        ctx.problem.set(None);
        busy.set(true);
        let id = ctx.agent_id.get_value();
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                "/api/op/agent_open_chat",
                &json!({ "id": id, "values": values }),
            )
            .await;
            busy.set(false);
            match res {
                Ok(v) => {
                    for w in v
                        .get("warnings")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        // Amber, not red: the thread was created. A warning
                        // here is "it opened, and here is what is off about
                        // it" — an unserved alias, a missing MCP server.
                        toasts.warn(w.to_string());
                    }
                    let url = v
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or("/chat")
                        .to_string();
                    if let Some(go) = navigate.try_get_value() {
                        go(&url, Default::default());
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

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
    let gaps = StoredValue::new(gaps);
    // §4.3's second warning list, beside the tool gaps and rendered in the same
    // place.
    let warnings = StoredValue::new(d.warnings.clone());
    let definition = tab_href(&d.id, "definition");

    // The last run, one click from its review — only while none is on screen.
    let last = Signal::derive(move || {
        runs.with(|r| {
            r.as_ref()
                .and_then(|r| r.as_ref().ok())
                .and_then(|l| l.first().cloned())
        })
    });
    let show_last =
        move || !is_chat && run.summary.with(Option::is_none) && last.with(Option::is_some);

    let (head, explain_summary, explain_body) = if is_chat {
        (
            "Chat",
            "Opening creates an ordinary Chat thread seeded from this manifest.",
            "The model in Config, the system prompt and the tools the agent attaches go with \
             it. It then behaves like any other thread, and lists under Threads here.",
        )
    } else if is_container {
        (
            "Container run",
            "A run starts the agent's image and writes nothing: the review is the gate.",
            "The image reports rows on stdout. Check the rows you want, fix what needs fixing, \
             then apply, which runs the same image again with LMGW_PHASE=apply.",
        )
    } else {
        (
            "Batch run",
            "A run lists, fetches and classifies, and writes nothing: the review is the gate.",
            "Check the rows you want, fix what the model got wrong, then apply.",
        )
    };

    view! {
        <section class="card run-card">
            <div class="card-head">
                <span class="mini-head">{head}</span>
            </div>
            <div class="run-acts">
                {if is_chat {
                    view! {
                        <button
                            class="btn primary"
                            disabled=move || busy.get() || ctx.blocked()
                            title=why
                            on:click=open_chat
                        >
                            {move || if busy.get() { "Opening…" } else { "Open in Chat" }}
                        </button>
                    }
                        .into_any()
                } else if is_container {
                    view! {
                        <button
                            class="btn primary"
                            title=why
                            disabled=move || off.get()
                            on:click=move |_| ctx.start("run")
                        >
                            "Start run"
                        </button>
                    }
                        .into_any()
                } else {
                    view! {
                        <button
                            class="btn"
                            title=move || {
                                if ctx.blocked() {
                                    why()
                                } else {
                                    "source and fetch only — no model call".to_string()
                                }
                            }
                            disabled=move || off.get()
                            on:click=move |_| ctx.start("list")
                        >
                            "List only"
                        </button>
                        {has_classify
                            .then(|| {
                                view! {
                                    <button
                                        class="btn primary"
                                        title=why
                                        disabled=move || off.get()
                                        on:click=move |_| ctx.start("classify")
                                    >
                                        "Dry run · classify"
                                    </button>
                                }
                            })}
                    }
                        .into_any()
                }}
            </div>
            // Named, because the form is right there: most of these are a
            // two-second fix, not a fault.
            {move || ctx.blocked_by.get().map(|w| view! { <div class="run-why">{w}</div> })}
            {(!is_chat).then(|| view! { <MountSummary fields=ctx.fields draft=ctx.draft/> })}
            <Show when=show_last>
                {move || {
                    last.get()
                        .map(|r| {
                            let job_id = r.job_id;
                            let when = short_ts(r.finished_at.as_deref().unwrap_or(&r.created_at));
                            view! {
                                <div class="run-last">
                                    <span class="dim">"Last run"</span>
                                    <span class="mono-sm">{format!("#{job_id} · {}", r.phase)}</span>
                                    <span class=status_chip(&r.status)>
                                        <span class="dot"></span>
                                        {r.status.clone()}
                                    </span>
                                    <span class="dim mono-sm">{when}</span>
                                    <button
                                        class="link-btn"
                                        title="show this run's rows here"
                                        on:click=move |_| {
                                            run.load(job_id);
                                            run.pinned.set(Some(job_id));
                                        }
                                    >
                                        "Open"
                                    </button>
                                </div>
                            }
                        })
                }}
            </Show>
            <Explain summary=explain_summary persist="agents.explain.run">
                {explain_body}
            </Explain>
        </section>

        <For each=move || gaps.get_value() key=|g| g.clone() let:g>
            <div class="notice warn">
                <b>{g}</b>
                <span class="detail">
                    "Register it on " <a href="/mcp-servers">"MCP servers"</a>
                    "; the agent is installed and waiting."
                </span>
            </div>
        </For>

        <For each=move || warnings.get_value() key=|w: &AgentWarning| w.code.clone() let:w>
            <div class=if w.blocks_start { "notice err" } else { "notice warn" }>
                <b>{w.message.clone()}</b>
                <span class="detail">{w.code.clone()}</span>
                // §5.1's way out lives on the Definition tab, with the rest of
                // what replaces the manifest; the notice points there.
                {(w.code == "builtin_update_available")
                    .then(|| {
                        let href = definition.clone();
                        view! {
                            <span class="detail">
                                "Reset to shipped is under " <a href=href>"Definition"</a> "."
                            </span>
                        }
                    })}
            </div>
        </For>

        // Whatever the run kind is: the image and its limits belong to a
        // container agent, but the *package* half belongs to any row installed
        // from an image — a `chat` agent can arrive that way too, and would
        // otherwise have nowhere to show where it came from.
        <RuntimeBlock
            id=d.id.clone()
            runtime=d.runtime.clone()
            provenance=d.provenance.clone()
            reload=reload
        />
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
        .map(|f| {
            if f.title.is_empty() {
                f.name.clone()
            } else {
                f.title.clone()
            }
        })
        .collect()
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
        "set {} in Config above — this agent ships no default for it",
        unfilled.join(" and ")
    ))
}
