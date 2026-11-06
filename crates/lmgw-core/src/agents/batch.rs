//! The `batch` run kind's executor (agent-catalog design §2.4, §4).
//!
//! A batch run is the mail workflow's shape, generalized and made durable:
//! **list** items with one step, **fetch** each one, **classify** each one with
//! a single structured model call, stop for a human at the **review** table,
//! and only then **apply**. Nothing writes before a person has looked (§1,
//! principle 4).
//!
//! **A run is a job.** [`JobKind::AgentRun`], keyed `agent:<id>`, so it is
//! durable across a reload, cancelable, on the live feed, and bounded to one
//! live run per agent by the partial unique index the jobs table already has.
//! The generic feed carries counts and a stage; the **rows never ride it** —
//! fifty rows of raw model replies on every 500 ms frame to every open
//! dashboard tab is the wrong pipe (§3). They sit in [`RunBuffer`] while the
//! run is in flight and in the job's `result` afterwards, both read by
//! `GET /api/agents/runs/{id}`.
//!
//! **One model path.** Every model turn here goes through
//! [`proxy::stream_once`], the same in-process path Admin Chat's `TurnRunner`
//! uses — never a raw POST. That is what closes the `TODO(one-path)` the mail
//! workflow left behind: the reason it hand-rolled its request was that
//! [`ChatRequest::passthrough`] did not exist when it was written, and the
//! structured-output constraint had nowhere else to ride (§4.2). A classify
//! through a question set is no model turn: it asks the set's decision model
//! through the real `/v1/systemone` handler in process ([`decide`],
//! labs-store design §4).
//!
//! **One tool path.** Every tool call goes through the resolved MCP surface
//! ([`mcp_exec::resolve`] + [`SplitExecutor`]), so the owner's per-tool switch,
//! the built-in `lmgw`/`docs` labels and the Logs rows all behave exactly as
//! they do for a Chat thread or a `/v1/responses` run — with
//! `ingress_proto = "agent-tool"` naming which surface asked (§4.3).
//!
//! **Nothing here invents a bound.** The tool-call and wall-clock budget is the
//! Responses pair under Settings → Agents & tools (§4.4). Concurrency is the manifest's, rendered
//! from the owner's config. The early-abort threshold is §2.4's `min(3, n)`.
//! A value that renders to something unusable fails the run saying so, rather
//! than falling back to a number nobody chose.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::agent::claim::media::MediaClaim;
use crate::agent::claim::{CallOn, LoopClaim};
use crate::agent::{
    self, Budget, Cancel, DeltaSink, LoopEvent, ResolvedTool, RunConfig, StopReason, ToolExecutor,
    TurnRunner,
};
use crate::config::Route;
use crate::erased::Erased;
use crate::error::GatewayError;
use crate::ingress::responses::{ApprovalRule, McpToolSpec};
use crate::ir::{
    flatten_tool_result, ChatRequest, Completion, ContentPart, Message, Params, Role,
    ToolResultBlock,
};
use crate::jobs::{self, JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use crate::mcp::exec::{
    self as mcp_exec, DocsExecutor, McpExecutor, SelfAdminExecutor, SplitExecutor,
};
use crate::proxy::{self, RequestCtx};
use crate::state::SharedState;
use crate::store;
use crate::telemetry::{AGENT_PROTO, AGENT_TOOL_PROTO};

use super::manifest::{BatchItem, ItemOutput, Manifest, RunSpec, Step, Turn};
use super::{template, Agent};

// ---------------------------------------------------------------------------
// Phases, input, rows
// ---------------------------------------------------------------------------

/// Which part of §2.4's lifecycle one job performs.
///
/// Four jobs rather than one long-lived one, because the review table sits
/// between them: a run that has classified is **done**, and Apply is a separate
/// decision a person makes afterwards — with its own row, its own cost and its
/// own failure, so retrying it never re-spends the classification (§4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// `source` + `fetch` + `columns`. No model call, no tokens (§2.4).
    List,
    /// `list`, then one structured model call per item.
    Classify,
    /// Re-classify only a finished run's attention rows, against the *current*
    /// config. Rows already classified keep their answer verbatim.
    Rerun,
    /// The only stage that writes: the `apply` step, with `rows` bound.
    Apply,
    /// The container runtime's own phase (container-runtime §3.2, §4): the
    /// whole run happens outside lmgw and reports through the ledger. Never
    /// driven by the batch pipeline below — [`Run::dispatch`] refuses it
    /// naming what does drive it.
    Run,
}

impl Phase {
    pub const ALL: [Phase; 5] = [
        Self::List,
        Self::Classify,
        Self::Rerun,
        Self::Apply,
        Self::Run,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Classify => "classify",
            Self::Rerun => "rerun",
            Self::Apply => "apply",
            Self::Run => "run",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// Every phase name, for an error that lists the alternatives.
    pub fn names() -> String {
        Self::ALL
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn classifies(self) -> bool {
        matches!(self, Self::Classify | Self::Rerun)
    }
}

/// The job's `input` column (§3). `agent_id` and `phase` are read straight out
/// of it by `api_agents::run_summary`, so their spelling is load-bearing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub agent_id: String,
    pub phase: Phase,
    /// `apply`: the reviewed, checked rows, with whatever the reviewer changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<Row>,
    /// `rerun`: the finished run whose attention rows are re-classified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_job: Option<i64>,
    /// This run is driven from outside: it was opened through
    /// `POST /api/agents/{id}/runs` and reports through the ledger
    /// (container-runtime §3.2), so the executor waits on events instead of
    /// running the manifest's pipeline.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ledger: bool,
    /// The Run tab's form at the moment Start was pressed: a sparse, **unsaved**
    /// patch over the agent's stored config, applied by
    /// [`Agent::override_config`](super::Agent::override_config) and never
    /// written back. The form is what a run uses; Save config is what sets the
    /// defaults it falls back to.
    ///
    /// Part of the job payload rather than read at start time, because a run is
    /// a job: the values that started it have to survive into the worker, and a
    /// re-run or an apply sends whatever the form holds at *that* click.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub values: Map<String, Value>,
    /// The merged, **non-secret** config this phase actually ran with — the
    /// stored values with [`Self::values`] over them, written when the run was
    /// started (mounts §5.7).
    ///
    /// Why it is recorded at all: an apply reruns days later against whatever
    /// the config says *then*, and the rows in front of the reviewer were
    /// produced by the config as it was. An apply started with `base_job`
    /// merges that run's `effective` over the stored config before its own
    /// `values`, so what is applied is what was reviewed. A general fix, not a
    /// mount one — a mount field is only the case where getting it wrong
    /// writes into the wrong folder.
    ///
    /// `None` on every row written before this existed, and they behave as they
    /// always did: an apply merges its own `values` over the stored config and
    /// nothing else.
    ///
    /// **It holds host paths**, so it goes nowhere near the run DTOs: a run
    /// summary is read by the agent itself (principals §3.2) and takes
    /// `agent_id` and `phase` out of this document and nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective: Option<Map<String, Value>>,
    /// The paired device whose `lmgw__agent_run` started this run, if one
    /// did (client-apps design L5's note, 2026-10-07): the run's calls of
    /// lmgw's admin tools are that device's, capped at what its admin tools
    /// may do when each call is made (read as stored), and filed as its
    /// rows. Nothing else of the run is the device's: its tools are the
    /// agent's own, its manifest's labels, resolved and called as the
    /// gateway's. `None` for the owner's runs and every row written before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<i64>,
}

/// One row of the review table.
///
/// Both a live frame and a stored result use this shape, so the table renders
/// the same whether the run is in flight or was finished last week.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Row {
    /// `item.id` rendered. Empty marks the row errored — there is no
    /// stable identity to apply anything against.
    // Design: agent-catalog §2.3.
    pub id: String,
    /// The **source** item, kept so a re-run can re-fetch and re-classify this
    /// row without re-listing the mailbox. Not shown anywhere.
    #[serde(skip_serializing_if = "Value::is_null")]
    pub item: Value,
    /// The manifest's `columns`, rendered. Order comes from the manifest, not
    /// from this map.
    pub columns: Map<String, Value>,
    /// The structured answer. `null` for a list-only run, which makes no call.
    pub output: Value,
    /// Exactly the `user` text the model received, for the details modal.
    /// Kept per row because "why did it say that" is the question the
    /// review table exists to answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// What the model actually replied, so a surprising answer is explainable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The output equals the fallback, or the call failed; through a question
    /// set also a confidence below the set's measured threshold ([`decide`]).
    pub attention: bool,
    /// The question set that classified this row, as the run read it
    /// ([`decide`]); `None` for the model call. "Save as labelled set" saves
    /// a run's rows into the set stamped on them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set: Option<lmgw_api_types::AgentRowSet>,
}

impl Row {
    /// The row carries something a call (or its failure) left: an answer, a
    /// prompt, a raw reply, an error or a set. A row a stage only listed or
    /// fetched, and never reached, carries none of it.
    fn asked(&self) -> bool {
        !self.output.is_null()
            || self.prompt.is_some()
            || self.raw.is_some()
            || self.error.is_some()
            || self.set.is_some()
    }

    /// What `{{rows}}` binds to in the apply step: the identity to write
    /// against and the (possibly overridden) output — `[{ id, <output
    /// fields…> }]`, and nothing else.
    ///
    /// **The review columns are deliberately not here** (decided 2026-09-18).
    /// They are rendered from whatever the source and fetch steps returned,
    /// which for the mail agent is a stranger's `From` and `Subject`; the apply
    /// turn is the one place in a run with write tools attached, so text an
    /// outsider chose must not enter its prompt to argue with the system
    /// message. The apply step is told *what to write*, not re-shown the run:
    /// no shipped prompt needs a column, and a manifest that genuinely does
    /// must declare it explicitly rather than widen this default for everyone.
    pub(super) fn for_apply(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), Value::String(self.id.clone()));
        if let Value::Object(out) = &self.output {
            for (k, v) in out {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
    }
}

// ---------------------------------------------------------------------------
// The live row buffer
// ---------------------------------------------------------------------------

/// Rows of the runs currently in flight, keyed by job id.
///
/// Lives on [`AppState`](crate::state::AppState) rather than in a `static` so
/// two gateways in one test process (which every integration test here is)
/// cannot see each other's rows through colliding job ids.
#[derive(Default)]
pub struct RunBuffer {
    rows: Mutex<HashMap<i64, Vec<Row>>>,
    /// The run log of a run still in flight (container-runtime §3.2).
    ///
    /// Beside the rows rather than on the ledger's `Desk`, because transport A
    /// keeps its own accumulator off the desk on purpose — registering it there
    /// would open the HTTP `events`/`close` routes onto a run lmgw is already
    /// driving. `AgentRunDetail.log` reads this while the run is live and the
    /// job's `result.log` once it has ended, exactly as it does for the rows.
    log: Mutex<HashMap<i64, Vec<String>>>,
}

impl RunBuffer {
    /// The rows of a run still in flight. `None` once it has finished — from
    /// then on the job's `result` is the answer.
    pub fn rows(&self, job_id: i64) -> Option<Vec<Row>> {
        self.rows.lock().unwrap().get(&job_id).cloned()
    }

    /// The run log of a run still in flight, same contract as [`Self::rows`].
    pub fn log(&self, job_id: i64) -> Option<Vec<String>> {
        self.log.lock().unwrap().get(&job_id).cloned()
    }

    /// Append one run-log line. An append rather than a publish: a chatty
    /// container writes one line at a time and re-cloning the whole log per
    /// line is the wrong cost.
    pub fn push_log(&self, job_id: i64, line: &str) {
        self.log
            .lock()
            .unwrap()
            .entry(job_id)
            .or_default()
            .push(line.to_string());
    }

    /// Publish a run's rows. Public because the ledger (container-runtime
    /// §3.2) writes here too: `rows_of` — and therefore the Run tab's review
    /// table — cannot tell a hand-posted run from an in-process one, and that
    /// is the point.
    pub fn put(&self, job_id: i64, rows: &[Row]) {
        self.rows.lock().unwrap().insert(job_id, rows.to_vec());
    }

    fn clear(&self, job_id: i64) {
        self.rows.lock().unwrap().remove(&job_id);
        self.log.lock().unwrap().remove(&job_id);
    }
}

// ---------------------------------------------------------------------------
// Starting and cancelling
// ---------------------------------------------------------------------------

/// Start a run, or report the one already running for this agent.
///
/// [`Spawn::AlreadyRunning`] is not an error: "one live run per agent" (§2.4)
/// is enforced by the `(kind, key)` index, and the honest answer to a second
/// Start is the job that is already doing it.
pub async fn start(state: &SharedState, input: Input) -> Result<Spawn, String> {
    let label = format!("{} · {}", input.agent_id, input.phase.as_str());
    let key = super::job_key(&input.agent_id);
    let payload = serde_json::to_value(&input).map_err(|e| e.to_string())?;
    jobs::spawn(state, JobKind::AgentRun, Some(key), label, payload).await
}

/// Ask this agent's running job to stop.
pub async fn cancel_for_agent(state: &SharedState, agent_id: &str) -> Result<String, String> {
    let job = state
        .jobs
        .live_by_key(JobKind::AgentRun, &super::job_key(agent_id))
        .ok_or_else(|| format!("no run is in flight for agent '{agent_id}'"))?;
    jobs::cancel(state, job.id).await
}

// ---------------------------------------------------------------------------
// The executor
// ---------------------------------------------------------------------------

pub struct AgentRunExecutor;

impl JobExecutor for AgentRunExecutor {
    fn kind(&self) -> JobKind {
        JobKind::AgentRun
    }

    /// The run's meter opens before a request stamped `X-Lmgw-Run` can name
    /// the run, which is once its job is live (§3.1).
    fn going_live(&self, state: &SharedState, id: i64) {
        state.agent_meters.open(id);
    }

    fn run<'a>(&'a self, ctx: JobCtx, input: Value) -> Erased<'a, Result<JobOutcome, String>> {
        Box::pin(async move {
            let input: Input = serde_json::from_value(input)
                .map_err(|e| format!("agent_run input (phase: {}): {e}", Phase::names()))?;
            let out = if input.ledger {
                super::ledger::execute(&ctx, &input).await
            } else {
                execute(&ctx, &input).await
            };
            // The result column has taken over from here, whichever way it ended.
            ctx.state.agent_runs.clear(ctx.id);
            // Same for the per-run meter: a run that is over cannot be stamped any
            // more, and a map entry nobody will ever read again is a leak.
            ctx.state.agent_meters.take(ctx.id);
            out
        })
    }
}

/// Everything one run holds for its whole life: the model path, the tool path,
/// and the bounds both are under.
///
/// Both planes are held as trait objects rather than as the concrete
/// [`AgentRunner`] and [`SplitExecutor`] [`execute`] builds. That is not
/// indirection for its own sake: it is what lets the §8 tests drive this whole
/// pipeline against a scripted model and a fake tool surface, with no HTTP, no
/// MCP server and no container anywhere — the same split `agent.rs` makes, for
/// the same reason.
struct Run<'a> {
    ctx: &'a JobCtx,
    agent: Agent,
    /// `None` for a phase that makes no model call — a list-only run must not
    /// need a model to browse a mailbox (§2.4).
    runner: Option<&'a dyn TurnRunner>,
    exec: &'a dyn ToolExecutor,
    tools: Vec<ResolvedTool>,
    budget: Budget,
    alias: String,
    base: template::Ctx,
    /// A re-run's full table. Its stages only ever see the attention rows, and
    /// publishing *those* would make the Run tab flicker down to the four rows
    /// being redone; with this set, every partial frame is spliced back into
    /// the whole run by row id before it is published.
    overlay: Mutex<Option<Vec<Row>>>,
    /// What the apply step produced, written as soon as the step returns —
    /// success, cancel or failure alike. A failed apply is reported from here
    /// (§4.1): the run has to be able to say what it wrote even when the thing
    /// that broke is the sentence the model was going to write about it.
    applied: Mutex<Option<Value>>,
    meter: Meter,
    /// The question set a classify or re-run classifies through, read once
    /// as it starts; `None` classifies with the model call (labs-store
    /// design §4).
    decider: Option<decide::Decider>,
}

/// Drives one model turn for a run: [`proxy::stream_once`], logged as
/// `ingress_proto = "agent"`, holding one GPU admission for the whole run the
/// way a tool loop does (§9b) rather than one per item — let go before an
/// image or audio tool call that needs the GPU and taken again by the next
/// model call (`agent::claim`).
struct AgentRunner {
    state: SharedState,
    route: Route,
    /// Why `route` is a fallback, when it is — every turn's row says so.
    fallback: Option<crate::gate::FallbackReason>,
    claim: std::sync::Arc<LoopClaim>,
}

impl TurnRunner for AgentRunner {
    fn run_turn<'a>(
        &'a self,
        ir: &'a ChatRequest,
        deadline: Duration,
        sink: &'a mut dyn DeltaSink,
    ) -> Erased<'a, Result<Completion, GatewayError>> {
        Box::pin(async move {
            let started = std::time::Instant::now();
            let call = match self.claim.call_on(&self.state, ir).await {
                Ok(call) => call,
                Err(r) => {
                    let route = r.route.as_deref().unwrap_or(&self.route);
                    let fallback = if r.gate { r.fallback } else { self.fallback };
                    crate::agent::claim::refused::record(
                        &self.state,
                        (proxy::KeyRef::default(), &ir.model_alias),
                        AGENT_PROTO,
                        route,
                        fallback,
                        started,
                        &r.error,
                    )
                    .await;
                    return Err(r.error);
                }
            };
            let (hold, route, fallback) = match &call {
                CallOn::Claim(hold) => (hold.as_ref(), &self.route, self.fallback),
                CallOn::Rerouted(o) => (o.hold.as_ref(), &o.route, o.headers.fallback_reason()),
            };
            proxy::stream_once(
                &self.state,
                hold,
                route,
                fallback,
                ir,
                AGENT_PROTO,
                proxy::KeyRef::default(),
                deadline.saturating_sub(started.elapsed()),
                sink,
            )
            .await
        })
    }
}

/// Load, validate, wire, dispatch. Erased ([`crate::erased`]): the run's
/// whole engine, which the job's `run` awaits.
fn execute<'a>(ctx: &'a JobCtx, input: &'a Input) -> Erased<'a, Result<JobOutcome, String>> {
    futures::FutureExt::boxed(execute_unerased(ctx, input))
}

async fn execute_unerased(ctx: &JobCtx, input: &Input) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    let row = store::get_agent(&state.db, &input.agent_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no agent with id '{}'", input.agent_id))?;
    let mut agent = Agent::from_row(row)?;
    // Before anything reads the config: the whole run — the model call, the
    // container's env, the token's scope — has to see the same values the
    // person was looking at when they pressed Start.
    agent.override_config(&input.values)?;
    // Validated again at run time: a row written by a newer build must not
    // crash an older one, it must fail the job with the reason (§4.1).
    agent.manifest.validate()?;
    if !agent.row.enabled {
        return Err(format!(
            "agent '{}' is disabled; enable it on the catalog before running it",
            input.agent_id
        ));
    }
    // One job kind, two run shapes (container-runtime §6): a container agent's
    // phases are driven by the runner next door, which reaches the same
    // `RunBuffer`, the same review table and the same job result. Routed here
    // rather than in the executor so both shapes share the load-validate-enable
    // preamble above — a disabled agent is refused identically either way.
    if matches!(agent.manifest.run, RunSpec::Container { .. }) {
        return super::container::execute(ctx, input, agent).await;
    }
    let RunSpec::Batch { item, apply, .. } = &agent.manifest.run else {
        return Err(format!(
            "'{}' is a {} agent; only the batch kind has runs",
            input.agent_id,
            agent.manifest.kind()
        ));
    };
    let fields = agent.manifest.fields().map_err(|e| e.join("; "))?;
    super::manifest::validate_values(&fields, &agent.config_values())?;

    // A `script` apply step is a container run of the script image (§4.2), and
    // it is independent of the run kind: this agent's classify stage is still a
    // model call per row, and only the step that *writes* leaves the process.
    // Routed after the config check, because the script reads `ctx.config`.
    if input.phase == Phase::Apply {
        if let Some(script) = apply.as_ref().and_then(|s| s.script.as_ref()) {
            if input.rows.is_empty() {
                return Err("nothing to apply: no rows were checked".to_string());
            }
            let (script, output) = (script.clone(), apply.as_ref().unwrap().output.clone());
            return super::container::execute_script(ctx, input, agent, &script, output.as_ref())
                .await;
        }
    }

    let base = template::Ctx {
        config: agent.effective_config(),
        ..Default::default()
    }
    .with_identity(&agent.manifest.id, &agent.manifest.name, Some(ctx.id));

    // A classify through a question set (labs-store design §4) is read and
    // checked before any tool call, like the model below: a set that is gone
    // or cannot answer the output field fails the run saying why.
    let decider = match (input.phase.classifies(), item.output.as_ref()) {
        (true, Some(out)) if item.user.is_some() => {
            decide::for_run(state, out, &base.config).await?
        }
        _ => None,
    };

    // Does this phase reach a model at all? Decided from the manifest, not
    // assumed: a list-only run and an apply that is a direct tool call both
    // spend no tokens and must not need a servable alias to start. A
    // classify through a set asks the set's model, not this one.
    let needs_model = match input.phase {
        Phase::List => false,
        Phase::Classify | Phase::Rerun => item.user.is_some() && decider.is_none(),
        Phase::Apply => apply.as_ref().is_some_and(Step::is_turn),
        // Refused by `dispatch` below, after the run has a result to fail
        // into. Nothing here reaches a model.
        Phase::Run => false,
    };
    // The model the run reports: the set's when a set classifies.
    let alias = match &decider {
        Some(d) => d.model.clone(),
        None => template::render_text(&agent.manifest.model.alias, &base)
            .trim()
            .to_string(),
    };

    // Before the tool plane is even resolved, let alone called: a run whose
    // model cannot be resolved is refused **before any tool call** (§8), so a
    // misconfigured agent never touches the outside world.
    let mut runner: Option<AgentRunner> = None;
    if needs_model {
        if alias.is_empty() {
            // A classify of an output that can decide through a set needs
            // the one or the other (labs-store design §4).
            let set_field = item
                .output
                .as_ref()
                .and_then(|o| o.decide_field())
                .filter(|_| input.phase.classifies());
            return Err(match set_field {
                Some(field) => format!(
                    "this agent has no model and no question set: '{}' rendered empty against \
                     the stored config, and '{field}' names no set. Pick a model or a question \
                     set on the Run tab.",
                    agent.manifest.model.alias
                ),
                None => format!(
                    "this agent has no model: '{}' rendered empty against the stored config. \
                     Pick one on the Run tab.",
                    agent.manifest.model.alias
                ),
            });
        }
        // The gate's per-request half: the hold swap, then one admission for
        // the whole run, like a tool loop's (§9b) — the concurrent classify
        // calls below all forward to the same container, each with its own
        // per-send lease (`proxy::stream_once`).
        //
        // A candidate alias refuses a capability facet it does not enable
        // before anything starts (candidate-aliases §4.6): a run whose
        // manifest attaches tools gives the model tools.
        let uses = if agent.manifest.tools.is_empty() {
            crate::candidates::FacetSet::EMPTY
        } else {
            crate::candidates::FacetSet::EMPTY.insert(crate::candidates::Facet::ToolCalls)
        };
        let crate::gate::Opened {
            route,
            hold: admission,
            headers,
        } = async {
            crate::gate::resolve(
                state,
                &alias,
                crate::gate::RouteCheck::None,
                crate::runtime::registry::Origin::Owner,
            )
            .await?
            .using(uses)?
            .admit(state)
            .await
        }
        .await
        .map_err(|f| format!("model '{alias}': {}", f.error))?;
        if let (Some(fb), Some(reason)) = (headers.fallback(), headers.fallback_reason()) {
            tracing::info!(
                agent = %agent.row.id,
                alias,
                fallback = %fb,
                reason = reason.as_str(),
                "agent run: re-routed to the fallback"
            );
        }
        runner = Some(AgentRunner {
            state: state.clone(),
            route,
            fallback: headers.fallback_reason(),
            claim: std::sync::Arc::new(LoopClaim::new(admission)),
        });
    }

    let (tools, builtin, offered, media_defaults) = resolve_tools(state, &agent.manifest).await?;
    // Every half of the tool plane logs under the same proto: a tool call made
    // by a run is an agent's tool call whether it landed on a registered MCP
    // server, on `lmgw` or on `docs` (§4.3). Without this the built-in labels
    // would file their rows under Admin Chat and /v1/responses.
    let mut exec = SplitExecutor::new(
        SelfAdminExecutor::new(state.clone(), run_caller(state, input.started_by))
            .with_proto(AGENT_TOOL_PROTO),
        DocsExecutor::new(state.clone(), RequestCtx::default())
            .with_client(format!("agent {}", agent.manifest.id))
            .with_proto(AGENT_TOOL_PROTO),
        builtin,
        McpExecutor::new(state.clone(), RequestCtx::default())
            .with_proto(AGENT_TOOL_PROTO)
            .with_caller(crate::mcp::host::CallFrom::gateway())
            .with_listed(offered),
    )
    // A call that leaves out `model` runs on the default the listed tool
    // named (chat-media-tools §3.8).
    .with_media_defaults(media_defaults);
    // An image or audio call that needs the GPU gets the run's room
    // (chat-media-tools §4.3).
    if let Some(r) = &runner {
        exec = exec.with_media_hooks(std::sync::Arc::new(MediaClaim::new(
            state.clone(),
            r.claim.clone(),
        )));
    }
    let settings = state.snapshot().settings.clone();
    let mut run = Run {
        ctx,
        agent,
        runner: runner.as_ref().map(|r| r as &dyn TurnRunner),
        exec: &exec,
        tools,
        budget: Budget {
            max_tool_calls: settings.responses_max_tool_calls,
            wall_clock: Duration::from_secs(settings.responses_timeout_seconds.max(1)),
        },
        alias,
        base,
        overlay: Mutex::new(None),
        applied: Mutex::new(None),
        meter: Meter::default(),
        decider,
    };
    run.dispatch(input).await
}

/// Whom a run's calls of lmgw's admin tools are made as: the paired device
/// that started it (`Input::started_by`, client-apps design L5's note,
/// 2026-10-07), so they are capped at what its admin tools may do as each
/// call is made, read from the stored key row and settings — `off` once the
/// device is gone, disabled or expired — and filed as its rows; the
/// gateway's own context otherwise. A device that is gone is still named by
/// its id, never left as the gateway. Only the self-admin half of the run's
/// executor takes it: the MCP and docs halves are the agent's own tools
/// (`resolve_tools`).
fn run_caller(state: &SharedState, started_by: Option<i64>) -> RequestCtx {
    let Some(id) = started_by else {
        return RequestCtx::default();
    };
    let principal = state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.id == id)
        .map(crate::principal::Principal::from_key)
        .unwrap_or(crate::principal::Principal::Key {
            id,
            name: format!("{}{id}", crate::devices::NAME_PREFIX),
            kind: crate::config::ApiKeyKind::Device,
            agent_id: None,
            fingerprint: String::new(),
        });
    let client_key = match &principal {
        crate::principal::Principal::Key { name, .. } => Some(name.clone()),
        crate::principal::Principal::Anonymous => None,
    };
    RequestCtx {
        principal,
        client_key,
        ..Default::default()
    }
}

/// The agent's `tools[]` against the MCP plane, with `require_approval: Never`
/// — the review table is the approval (§4.3).
///
/// A label that does not resolve **fails the run**, naming it and the available
/// ones: a batch step calls a tool by name, so a missing server is not a
/// degraded run, it is a run that cannot take its first step.
/// With the tools: the built-in names, the server each other name was
/// listed from — the only names the run's executor calls — and the media
/// defaults the list named.
async fn resolve_tools(state: &SharedState, m: &Manifest) -> Result<ResolvedTools, String> {
    if m.tools.is_empty() {
        return Ok(Default::default());
    }
    let specs: Vec<McpToolSpec> = m
        .tools
        .iter()
        .map(|t| McpToolSpec {
            server_label: t.label.clone(),
            allowed_tools: t.allowed.clone(),
            require_approval: ApprovalRule::Never,
            server_url: None,
            connector_id: None,
        })
        .collect();
    // An in-process run uses the agent's own tool scope, its manifest's
    // labels, whoever started it: a run a device started resolves and calls
    // them as the gateway does, and only its calls of lmgw's admin tools are
    // capped at the device's level (`run_caller`). A device may write an
    // agent only at `full`, where it can also widen its own scope (the
    // pre-merge review's P-3), so the device's tool scope is not applied
    // here (the branch review's verification, V-6).
    // The manifest's labels, which the owner installed: a failure says its
    // real reason, as a thread's attached label does.
    let resolved =
        mcp_exec::resolve_attached(state, &specs, &crate::mcp::scope::ToolScope::gateway()).await;
    if let Some(f) = resolved.failed.first() {
        return Err(format!("MCP server '{}': {}", f.label, f.why));
    }
    Ok((
        resolved.tools,
        resolved.builtin,
        resolved.servers,
        resolved.media_defaults,
    ))
}

/// [`resolve_tools`]' answer: with the media tools' default models the
/// listed tools name.
type ResolvedTools = (
    Vec<ResolvedTool>,
    Vec<String>,
    HashMap<String, i64>,
    lmgw_api_types::chat_media::MediaDefaults,
);

// ---------------------------------------------------------------------------
// One step: a direct call or a turn (§2.2)
// ---------------------------------------------------------------------------

/// What a step produced.
#[derive(Debug)]
struct StepOut {
    /// The step's result as data: a direct call's JSON, a turn's structured
    /// `output`, or — for a turn with no `output` — its final text as a string.
    value: Value,
    meter: Meter,
    /// One entry per tool the turn executed, for the apply result block (§6.2).
    tool_calls: Vec<Value>,
    /// The turn's final text, kept so a caller can show what was said.
    text: String,
}

impl StepOut {
    fn data(value: Value, meter: Meter) -> Self {
        Self {
            value,
            meter,
            tool_calls: Vec::new(),
            text: String::new(),
        }
    }
}

/// A step that produced no result, **and what it had already done**.
///
/// A turn that made eight successful tool calls and then lost the upstream
/// connection on its ninth model call has still made those eight calls. The
/// message alone says what broke; `partial` says what changed, which is the
/// half the owner needs before deciding whether to retry (§4.1, §4.6). Every
/// caller either folds it into the run or explicitly drops it — it cannot be
/// lost by writing `?`.
#[derive(Debug)]
struct StepFail {
    error: String,
    partial: StepOut,
}

impl StepFail {
    /// A failure with nothing behind it: the step never reached a tool or a
    /// model.
    fn bare(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            partial: StepOut::data(Value::Null, Meter::default()),
        }
    }

    fn with(error: impl Into<String>, partial: StepOut) -> Self {
        Self {
            error: error.into(),
            partial,
        }
    }
}

/// What a turn has done so far, folded **as the events arrive**.
///
/// Folding afterwards out of a collected event list is the bug this replaces:
/// the list is owned by the loop's caller, and on the error path the caller
/// never gets to read it, so a turn that failed reported zero tool calls
/// however many it had made. Accumulating here means every exit — done,
/// cancelled, budget, transport error — reads the same numbers from the same
/// place (§4.1, §4.6).
#[derive(Debug, Default)]
struct Seen {
    tool_calls: Vec<Value>,
    text: String,
}

impl Seen {
    fn note(&mut self, ev: &LoopEvent) {
        match ev {
            // A batch's answer has no refusal of its own: its text.
            LoopEvent::Text(t) | LoopEvent::Refusal(t) => self.text.push_str(t),
            LoopEvent::CallResult {
                name,
                is_error,
                ms,
                blocks,
                ..
            } => {
                let (out, _) = flatten_tool_result(blocks);
                self.tool_calls.push(json!({
                    "name": name,
                    "ok": !is_error,
                    "ms": ms,
                    "output": out,
                }));
            }
            _ => {}
        }
    }

    /// The step this turn amounts to, whatever ended it. The usage, the cost
    /// and the model calls come from the runner's own tally rather than from
    /// the loop's return value, for the same reason the rest of this does.
    fn step(&self, value: Value, metered: &MeteredRunner) -> StepOut {
        StepOut {
            value,
            meter: Meter {
                tool_calls: self.tool_calls.len() as u32,
                ..metered.tally()
            },
            tool_calls: self.tool_calls.clone(),
            text: self.text.clone(),
        }
    }
}

/// An [`EventSink`](agent::EventSink) that folds the loop's events into
/// [`Seen`] and lets the owner stop it: returning `false` is how a tool loop is
/// aborted, and a cancelled apply must stop between tool calls rather than run
/// to completion (§4.1).
struct RunSink<'a> {
    ctx: &'a JobCtx,
    seen: Seen,
}

impl agent::EventSink for RunSink<'_> {
    fn emit<'a>(&'a mut self, ev: LoopEvent) -> Erased<'a, bool> {
        Box::pin(async move {
            self.seen.note(&ev);
            !self.ctx.canceled()
        })
    }
}

/// The classify step through a question set (labs-store design §4).
pub mod decide;

/// One item's classify, as [`Run::classify_stage`]'s loop reads it.
struct Classified {
    row: Row,
    /// The HTTP status of a *failed* call, which is what tells "the model is
    /// unreachable" from "this one item went wrong" (§2.4's early abort
    /// counts it at 5xx).
    failed: Option<u16>,
    meter: Meter,
    /// The model answered, which switches the early abort off.
    answered: bool,
    /// lmgw refused the call with what it would answer every item
    /// ([`decide`]): the stage stops and the run fails with it.
    stop: Option<String>,
}

impl Classified {
    /// `row` as it is, no call made or none that counts.
    fn left(row: Row) -> Self {
        Self {
            row,
            failed: None,
            meter: Meter::default(),
            answered: false,
            stop: None,
        }
    }
}

mod meter;
use meter::Meter;

mod metered;
use metered::MeteredRunner;

#[cfg(test)]
mod meter_tests;

impl Run<'_> {
    /// The manifest's sampling knobs, minus `max_tokens`: the model's context
    /// is known from its own selection and the run uses it (§2.1).
    fn params(&self) -> Params {
        let m = &self.agent.manifest.model;
        Params {
            temperature: m.temperature,
            top_p: m.top_p,
            top_k: m.top_k,
            seed: m.seed,
            reasoning: m.reasoning.clone(),
            ..Default::default()
        }
    }

    async fn progress(&self, stage: &str, done: u64, total: Option<u64>, detail: Value) {
        self.ctx
            .progress(JobProgress {
                done,
                total,
                stage: stage.to_string(),
                detail,
            })
            .await;
    }

    /// `{agent_id, phase, attention, errors}` — counts and nothing more. The
    /// rows themselves never ride the generic jobs feed (§3).
    fn detail(&self, phase: Phase, rows: &[Row]) -> Value {
        json!({
            "agent_id": self.agent.row.id,
            "agent_name": self.agent.manifest.name,
            "phase": phase.as_str(),
            "model": self.alias,
            "attention": rows.iter().filter(|r| r.attention).count(),
            "errors": rows.iter().filter(|r| r.error.is_some()).count(),
        })
    }

    /// Publish the rows a reader of `GET /api/agents/runs/{id}` would want
    /// right now. Cheap by design: a clone of a `Vec<Row>` behind a mutex, read
    /// only when someone has the Run tab open.
    fn publish(&self, rows: &[Row]) {
        let merged = match self.overlay.lock().unwrap().as_ref() {
            Some(base) => {
                let mut full = base.clone();
                // A re-run's rows not re-asked (yet, or at all when it
                // stops early) keep the base run's values.
                for fresh in rows.iter().filter(|r| !r.id.is_empty() && r.asked()) {
                    if let Some(slot) = full.iter_mut().find(|b| b.id == fresh.id) {
                        *slot = fresh.clone();
                    }
                }
                full
            }
            None => rows.to_vec(),
        };
        self.ctx.state.agent_runs.put(self.ctx.id, &merged);
    }

    /// The concurrency the manifest asks for, rendered from the owner's config.
    ///
    /// Absent means one at a time — the absence of parallelism, not a guessed
    /// ceiling. A template that renders to anything but a positive integer
    /// fails the run saying what it rendered to, rather than quietly becoming
    /// some number nobody chose (§14).
    fn concurrency(&self, item: &BatchItem) -> Result<usize, String> {
        let Some(raw) = &item.concurrency else {
            return Ok(1);
        };
        let v = template::render_value(raw, &self.base);
        match v.as_u64() {
            Some(n) if n >= 1 => Ok(n as usize),
            _ => Err(format!(
                "run.item.concurrency rendered to {v}, which is not a positive integer — set it \
                 on the Run tab"
            )),
        }
    }

    /// One step (§2.2): a direct tool call, or a turn of the tool loop.
    async fn run_step(
        &self,
        step: &Step,
        tctx: &template::Ctx,
        at: &str,
    ) -> Result<StepOut, StepFail> {
        match (&step.tool, &step.turn) {
            (Some(tool), _) => {
                let args = match &step.args {
                    Some(a) => template::render_value(a, tctx),
                    None => json!({}),
                };
                self.direct_call(tool, args).await
            }
            (None, Some(turn)) => self.turn_step(turn, tctx, at).await,
            // `Manifest::errors` refuses this shape at save and at import; a
            // row written by a newer build could still carry it.
            (None, None) => Err(StepFail::bare(format!(
                "{at} is neither a tool call nor a turn"
            ))),
        }
    }

    /// This run's cancel, as something an await can be raced against rather
    /// than something checked between them (§4.1).
    fn cancel(&self) -> Cancel {
        Cancel::flag(self.ctx.cancel_signal())
    }

    /// A direct call, read as **data** (§2.2): the MCP `structuredContent`
    /// block if there is one, else the first text block parsed as JSON. A tool
    /// that answers in prose fails the step rather than having the engine guess
    /// at a format.
    async fn direct_call(&self, tool: &str, args: Value) -> Result<StepOut, StepFail> {
        if !self.tools.iter().any(|t| t.def.name == tool) {
            return Err(StepFail::bare(format!(
                "no tool named '{tool}' is available to this agent (it has: {})",
                self.tool_names()
            )));
        }
        // Raced against the cancel: an MCP call can block for as long as the
        // server it fronts wants to, and dropping the future here is what ends
        // it. The null result is safe because **every** caller re-checks
        // `canceled()` before reading a step's value — `classify_run` and
        // `fetch_one` say so where they do it — so a cancelled step is never
        // mistaken for a tool that answered null. The call itself is still
        // recorded: it was sent, and dropping the future does not un-send it.
        let Some(outcome) = self.cancel().guard(self.exec.call(tool, &args)).await else {
            return Ok(StepOut {
                value: Value::Null,
                meter: Meter {
                    tool_calls: 1,
                    ..Default::default()
                },
                tool_calls: vec![json!({
                    "name": tool,
                    "ok": false,
                    "ms": 0,
                    "output": agent::ABANDONED_CALL,
                })],
                text: String::new(),
            });
        };
        let meter = Meter {
            tool_calls: 1,
            ..Default::default()
        };
        let (text, _) = flatten_tool_result(&outcome.blocks);
        if outcome.is_error {
            return Err(StepFail::with(
                format!("the tool '{tool}' failed: {text}"),
                // The call was made and it counts, however it answered.
                StepOut::data(Value::Null, meter),
            ));
        }
        // The host's copy of `structuredContent` first: the model's blocks
        // carry it only when the result's `content` was empty
        // (`blocks_from_result`). Then a JSON block — a tool lmgw runs
        // itself answers in one.
        if let Some(value) = outcome.structured.as_ref().or_else(|| {
            outcome.blocks.iter().rev().find_map(|b| match b {
                ToolResultBlock::Json { value } => Some(value),
                _ => None,
            })
        }) {
            return Ok(StepOut::data(value.clone(), meter));
        }
        let first_text = outcome.blocks.iter().find_map(|b| match b {
            ToolResultBlock::Text { text } => Some(text.as_str()),
            _ => None,
        });
        match first_text.and_then(|t| serde_json::from_str::<Value>(t.trim()).ok()) {
            Some(v) => Ok(StepOut::data(v, meter)),
            None => Err(StepFail::with(
                format!(
                    "the tool '{tool}' returned text that is not JSON; a batch step needs a JSON \
                     result"
                ),
                StepOut::data(Value::Null, meter),
            )),
        }
    }

    fn tool_names(&self) -> String {
        if self.tools.is_empty() {
            return "no tools at all — its manifest attaches none".to_string();
        }
        self.tools
            .iter()
            .map(|t| t.def.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A turn: the existing tool loop with exactly `tools` attached, bounded by
    /// the Responses pair (Settings → Agents & tools), then the output enforced in two stages
    /// (§2.2, §4.4).
    async fn turn_step(
        &self,
        turn: &Turn,
        tctx: &template::Ctx,
        at: &str,
    ) -> Result<StepOut, StepFail> {
        let runner = self.runner.ok_or_else(|| {
            StepFail::bare(format!(
                "{at} is a turn, which needs a model, and none was resolved"
            ))
        })?;
        let mut tools: Vec<ResolvedTool> = Vec::with_capacity(turn.tools.len());
        for want in &turn.tools {
            match self.tools.iter().find(|t| &t.def.name == want) {
                Some(t) => tools.push(t.clone()),
                None => {
                    return Err(StepFail::bare(format!(
                        "{at}.turn names the tool '{want}', which this agent cannot reach (it \
                         has: {})",
                        self.tool_names()
                    )))
                }
            }
        }

        let mut messages: Vec<Message> = Vec::new();
        if let Some(s) = &turn.system {
            let rendered = template::render_text(s, tctx);
            if !rendered.trim().is_empty() {
                messages.push(Message::text(Role::System, rendered));
            }
        }
        messages.push(Message::text(
            Role::User,
            template::render_text(&turn.prompt, tctx),
        ));
        let ir = ChatRequest {
            model_alias: self.alias.clone(),
            messages,
            params: self.params(),
            tools: Vec::new(),
            tool_choice: None,
            stream: false,
            passthrough: Default::default(),
            response_format: None,
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
            context_management: None,
            compaction: None,
            thinking_display: None,
        };

        let mut sink = RunSink {
            ctx: self.ctx,
            seen: Seen::default(),
        };
        // Wrapped so the tokens are tallied at the call: `agent::run` reports
        // usage on its `Ok` return only, and the paths below that do not get
        // one still have to price what they spent (§4.5).
        let metered = MeteredRunner::new(runner);
        let outcome = agent::run(
            ir,
            // `parallel_tool_calls: false` (§4.4): an apply step's calls write,
            // and writes that share a resource must not be reordered.
            RunConfig::new(tools, self.budget, false).with_cancel(self.cancel()),
            &metered,
            self.exec,
            &mut sink,
        )
        .await;
        // Every exit below reads the same two accumulators, so none of them can
        // report a turn as having done less than it did.
        let step = |value: Value| sink.seen.step(value, &metered);
        let result = match outcome {
            Ok(r) => r,
            Err(e) => return Err(StepFail::with(format!("{at}: {e}"), step(Value::Null))),
        };
        let (mut meter, tool_calls, text) = {
            let s = step(Value::Null);
            (s.meter, s.tool_calls, s.text)
        };

        match result.reason {
            StopReason::Done(_) => {}
            // Cancel is what stopped it: either the loop abandoned a call
            // mid-flight, or [`RunSink`] returned false between events. Not a
            // budget stop and not a failure — hand back what it did before
            // stopping, so a cancelled apply can still say which rows it wrote
            // (§4.1) instead of failing with a message about a budget nobody
            // hit.
            StopReason::Incomplete(_) if self.ctx.canceled() => {
                return Ok(StepOut {
                    value: Value::Null,
                    meter,
                    tool_calls,
                    text,
                })
            }
            // Named verbatim, because the two numbers it refers to are printed
            // next to the Apply button (§4.4).
            StopReason::Incomplete(why) => {
                return Err(StepFail::with(
                    format!(
                        "{at} stopped early: {why} — the budget is {} tool calls and {} s \
                         (Settings → Agents & tools)",
                        self.budget.max_tool_calls,
                        self.budget.wall_clock.as_secs()
                    ),
                    StepOut {
                        value: Value::Null,
                        meter,
                        tool_calls,
                        text,
                    },
                ))
            }
            // Neither can happen here: every tool is server-side and nothing is
            // gated (§4.3). Reported rather than assumed away.
            StopReason::ClientTool | StopReason::Approval => {
                return Err(StepFail::with(
                    format!(
                        "{at} stopped waiting for the caller, which an agent run has no way to \
                         answer"
                    ),
                    StepOut {
                        value: Value::Null,
                        meter,
                        tool_calls,
                        text,
                    },
                ))
            }
        }

        let Some(schema) = &turn.output else {
            return Ok(StepOut {
                value: Value::String(text.clone()),
                meter,
                tool_calls,
                text,
            });
        };
        // Stage one: the final answer, parsed against the schema.
        if let Some(v) = first_json_object(&text).filter(|v| matches_schema(v, schema).is_ok()) {
            return Ok(StepOut {
                value: v,
                meter,
                tool_calls,
                text,
            });
        }
        // Stage two: **one** further call, no tools, `response_format` carrying
        // the schema. A JSON-schema grammar and tool-call syntax cannot be
        // active on the same generation, which is why this is a separate call
        // rather than the whole turn being constrained (§2.2).
        let mut messages = result.messages;
        messages.push(Message::text(
            Role::User,
            "State the result of what you just did as a single JSON object matching the \
             required schema. No prose, no code fence.",
        ));
        let mut ir = ChatRequest {
            model_alias: self.alias.clone(),
            messages,
            params: self.params(),
            tools: Vec::new(),
            tool_choice: None,
            stream: false,
            passthrough: Default::default(),
            response_format: None,
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
            context_management: None,
            compaction: None,
            thinking_display: None,
        };
        // A turn's `output` is always the author's own schema.
        ir.response_format = Some(response_format(schema, "agent_output", false));
        // Same race as the turn itself. This is the call the runaway happened
        // on: a reasoning model with no tools left to call and a long answer to
        // think about, which used to be stoppable only by the per-turn deadline.
        // Metered like the loop's turns: its row's cost, however it ended.
        let finalize = MeteredRunner::new(runner);
        let finalized = self
            .cancel()
            .guard(finalize.run_turn(&ir, self.budget.wall_clock, &mut Discard))
            .await;
        let Some(completion) = finalized else {
            return Ok(StepOut {
                value: Value::Null,
                meter,
                tool_calls,
                text,
            });
        };
        meter.absorb(finalize.tally());
        let fail = |why: String, meter: Meter, tool_calls: Vec<Value>, text: String| {
            StepFail::with(
                why,
                StepOut {
                    value: Value::Null,
                    meter,
                    tool_calls,
                    text,
                },
            )
        };
        let completion = match completion {
            Ok(c) => c,
            Err(e) => {
                return Err(fail(
                    format!("{at}: the finalize call failed: {e}"),
                    meter,
                    tool_calls,
                    text,
                ))
            }
        };
        let reply = completion_text(&completion);
        match first_json_object(&reply) {
            Some(v) => match matches_schema(&v, schema) {
                Ok(()) => Ok(StepOut {
                    value: v,
                    meter,
                    tool_calls,
                    text,
                }),
                Err(why) => Err(fail(
                    format!(
                        "{at}: the model's structured output does not match the declared schema \
                         ({why}); it answered: {}",
                        reply.trim()
                    ),
                    meter,
                    tool_calls,
                    text,
                )),
            },
            None => Err(fail(
                format!(
                    "{at}: the model did not state a JSON result even when asked directly; it \
                     answered: {}",
                    reply.trim()
                ),
                meter,
                tool_calls,
                text,
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// The phases (§2.4)
// ---------------------------------------------------------------------------

impl Run<'_> {
    /// Drive the phase this job was started for and, whatever happens, report
    /// what the run did.
    ///
    /// A phase returning `Err` used to end the job with a message and a NULL
    /// result: the rows it had classified and the writes an apply had already
    /// made went with it. They are all still here — the rows in the live buffer
    /// this run has been publishing to, the apply's tool calls in
    /// [`Run::applied`] — so the failure carries them exactly as a cancel does
    /// (§4.1, §4.6).
    async fn dispatch(&mut self, input: &Input) -> Result<JobOutcome, String> {
        let out = match input.phase {
            Phase::List | Phase::Classify => self.classify_run(input.phase).await,
            Phase::Rerun => self.rerun(input).await,
            Phase::Apply => self.apply(input).await,
            // `run` is the container runtime's phase: nothing in lmgw drives
            // it, something outside does and reports through the ledger.
            Phase::Run => Err(format!(
                "phase 'run' is driven from outside lmgw — open it with \
                 POST /api/agents/{}/runs and report through the ledger",
                self.agent.row.id
            )),
        };
        match out {
            Err(error) => match self.failure_result(input.phase) {
                Some(value) => Ok(JobOutcome::FailedWith { error, value }),
                None => Err(error),
            },
            ok => ok,
        }
    }

    /// The `result` a failed run still stores, or `None` when it never got far
    /// enough to have one.
    ///
    /// The rows come from the live buffer rather than from a local, because the
    /// phase that failed took its locals with it — and the buffer is what the
    /// Run tab was already reading. A run refused before it did anything (a
    /// manifest this build cannot drive, an apply with nothing checked) keeps
    /// the NULL result it has always had: a report of zeroes is not more
    /// informative than no report, it just looks like one.
    fn failure_result(&self, phase: Phase) -> Option<Value> {
        let rows = self
            .ctx
            .state
            .agent_runs
            .rows(self.ctx.id)
            .unwrap_or_default();
        let applied = self.applied.lock().unwrap().clone();
        if rows.is_empty() && applied.is_none() && self.meter == Meter::default() {
            return None;
        }
        Some(self.result(phase, &rows, applied, false))
    }

    /// `list` and `classify`: the same pipeline, with the model half switched
    /// off for the first. A list-only run is still a job, still read-only, and
    /// spends nothing — which is the point of it (§2.4).
    async fn classify_run(&mut self, phase: Phase) -> Result<JobOutcome, String> {
        let RunSpec::Batch {
            source,
            items_path,
            item,
            ..
        } = self.agent.manifest.run.clone()
        else {
            return Err("this agent has no batch run".to_string());
        };
        if phase.classifies() && item.user.is_none() {
            return Err(
                "this agent declares no classify step (run.item.user), so it can only list"
                    .to_string(),
            );
        }

        self.progress("listing", 0, None, self.detail(phase, &[]))
            .await;
        let src = match self.run_step(&source, &self.base, "run.source").await {
            Ok(out) => out,
            Err(f) => {
                // Counted before the run ends: the source's call was made and
                // its tokens were spent whether or not it answered (§4.5).
                self.meter.absorb(f.partial.meter);
                return Err(f.error);
            }
        };
        self.meter.absorb(src.meter);
        // Before the result is read, not after: a source *turn* that cancel cut
        // short has no items to give, and "the source returned null, not an
        // array" would blame the manifest for something the owner did.
        if self.ctx.canceled() {
            return Ok(JobOutcome::CanceledWith(self.result(
                phase,
                &[],
                None,
                true,
            )));
        }
        let items = items_from(&src.value, items_path.as_deref())?;

        let (rows, fetched, meter, canceled) = self.fetch_stage(phase, &item, items).await?;
        self.meter.absorb(meter);
        if canceled {
            return Ok(JobOutcome::CanceledWith(
                self.result(phase, &rows, None, true),
            ));
        }
        if !phase.classifies() {
            self.publish(&rows);
            return Ok(JobOutcome::Done(self.result(phase, &rows, None, false)));
        }

        let pairs: Vec<(usize, Row, Value)> = rows
            .into_iter()
            .zip(fetched)
            .enumerate()
            .map(|(i, (r, f))| (i, r, f))
            .collect();
        let staged = self.classify_stage(phase, &item, pairs).await;
        let (rows, canceled) = self.fold_stage(staged)?;
        if canceled {
            return Ok(JobOutcome::CanceledWith(
                self.result(phase, &rows, None, true),
            ));
        }
        self.publish(&rows);
        Ok(JobOutcome::Done(self.result(phase, &rows, None, false)))
    }

    /// Per item, bounded by `concurrency`: render the id, run `fetch`, render
    /// the review columns. No model, no writes.
    async fn fetch_stage(
        &self,
        phase: Phase,
        item: &BatchItem,
        items: Vec<Value>,
    ) -> Result<(Vec<Row>, Vec<Value>, Meter, bool), String> {
        let conc = self.concurrency(item)?;
        let total = items.len() as u64;
        self.progress("fetching", 0, Some(total), self.detail(phase, &[]))
            .await;

        let mut slots: Vec<Option<(Row, Value, Meter)>> = (0..items.len()).map(|_| None).collect();
        let mut canceled = false;
        {
            let mut s = stream::iter(
                items
                    .into_iter()
                    .enumerate()
                    .map(|(i, it)| async move { (i, self.fetch_one(item, it).await) }),
            )
            .buffer_unordered(conc);
            let mut done = 0u64;
            while let Some((i, out)) = s.next().await {
                slots[i] = Some(out);
                done += 1;
                let so_far: Vec<Row> = slots.iter().flatten().map(|(r, _, _)| r.clone()).collect();
                self.publish(&so_far);
                self.progress("fetching", done, Some(total), self.detail(phase, &so_far))
                    .await;
                // Between items, which is the boundary a fetch can stop
                // cleanly at: dropping the stream cancels the calls in flight.
                if self.ctx.canceled() {
                    canceled = true;
                    break;
                }
            }
        }

        let mut rows = Vec::with_capacity(slots.len());
        let mut fetched = Vec::with_capacity(slots.len());
        let mut meter = Meter::default();
        for slot in slots.into_iter().flatten() {
            rows.push(slot.0);
            fetched.push(slot.1);
            meter.absorb(slot.2);
        }
        Ok((rows, fetched, meter, canceled))
    }

    async fn fetch_one(&self, item: &BatchItem, source_item: Value) -> (Row, Value, Meter) {
        let mut tctx = self.base.clone();
        tctx.item = source_item.clone();
        let id = template::render_text(&item.id, &tctx).trim().to_string();
        let mut row = Row {
            id: id.clone(),
            item: source_item,
            ..Default::default()
        };
        let mut meter = Meter::default();
        if id.is_empty() {
            // §2.3: an absent `item.id` marks the row errored. No fetch and no
            // classify follow — there is nothing Apply could address, so
            // spending a call on it would buy nothing.
            row.error = Some(format!(
                "this item has no id: '{}' rendered empty, so nothing can be applied to it",
                item.id
            ));
            row.attention = true;
        } else if let Some(step) = &item.fetch {
            match self.run_step(step, &tctx, "run.item.fetch").await {
                // A fetch the cancel abandoned answers null (see
                // `direct_call`). The row stays exactly as it was listed rather
                // than being shown with review columns rendered from a result
                // that never came back (§4.1).
                Ok(out) if self.ctx.canceled() => {
                    meter.absorb(out.meter);
                    return (row, Value::Null, meter);
                }
                Ok(out) => {
                    meter.absorb(out.meter);
                    tctx.fetched = out.value;
                }
                Err(f) => {
                    meter.absorb(f.partial.meter);
                    row.error = Some(f.error);
                    row.attention = true;
                }
            }
        }
        for (col, tmpl) in item.columns.iter() {
            row.columns.insert(
                col.clone(),
                Value::String(template::render_text(tmpl, &tctx)),
            );
        }
        let fetched = tctx.fetched.clone();
        (row, fetched, meter)
    }

    /// A classify stage's outcome with its meter folded into the run's either
    /// way: an aborted stage's calls were made and spent (§4.5), and the
    /// failed run's result reads them from here ([`Run::failure_result`]).
    fn fold_stage(
        &mut self,
        staged: Result<(Vec<Row>, Meter, bool), (String, Meter)>,
    ) -> Result<(Vec<Row>, bool), String> {
        match staged {
            Ok((rows, meter, canceled)) => {
                self.meter.absorb(meter);
                Ok((rows, canceled))
            }
            Err((error, meter)) => {
                self.meter.absorb(meter);
                Err(error)
            }
        }
    }

    /// One structured model call per item, bounded by `concurrency`, with
    /// §2.4's early abort in the result loop. An abort hands back what the
    /// calls it made spent, as a `StepFail` does: they were made whether or
    /// not the run goes on (§4.5).
    async fn classify_stage(
        &self,
        phase: Phase,
        item: &BatchItem,
        pairs: Vec<(usize, Row, Value)>,
    ) -> Result<(Vec<Row>, Meter, bool), (String, Meter)> {
        let conc = self.concurrency(item).map_err(|e| (e, Meter::default()))?;
        // Every row as listed, so a run that aborts or is cancelled still
        // reports the rows it had rather than losing the ones it never reached.
        let mut rows: Vec<Row> = pairs.iter().map(|(_, r, _)| r.clone()).collect();
        let total = rows.len() as u64;
        self.progress("classifying", 0, Some(total), self.detail(phase, &rows))
            .await;

        // All-fail-before-any-success is the "the model is down" signal, and
        // §2.4 fixes the window at min(3, n): one transient 5xx on an otherwise
        // healthy model must not abort a whole run.
        let abort_after = rows.len().clamp(1, 3);
        let mut any_ok = false;
        let mut infra_errs = 0usize;
        let mut fatal_seen: Option<String> = None;
        let mut fatal: Option<String> = None;
        let mut stopped: Option<String> = None;
        let mut canceled = false;
        let mut meter = Meter::default();
        {
            let mut s = stream::iter(pairs.into_iter().map(|(i, row, fetched)| async move {
                (i, self.classify_one(item, row, fetched).await)
            }))
            .buffer_unordered(conc);
            let mut done = 0u64;
            while let Some((i, c)) = s.next().await {
                let Classified {
                    row,
                    failed: fail,
                    meter: m,
                    answered,
                    stop,
                } = c;
                // `answered`: a row the model actually answered. A row that
                // was *skipped* (no id, or its fetch already failed) reports
                // no failure either, and counting that as a success would
                // switch the early abort off for the whole run — one
                // unfetchable first item and a dead model would tag all fifty
                // rows `fallback`, which is the outcome §2.4 exists to
                // prevent.
                meter.absorb(m);
                // lmgw's refusal that every row would get (a question set's
                // call, `decide`): no row's answer. The stage stops at once,
                // dropping the calls in flight as the early abort does, and
                // the run fails with it; the rows answered so far stay, as a
                // cancel keeps them.
                if let Some(why) = stop {
                    stopped = Some(format!(
                        "stopped at row {}: {why}. The {done} of {total} rows done before it are \
                         kept; start the run again once that is fixed",
                        row.id
                    ));
                    break;
                }
                let failed = row.error.clone();
                rows[i] = row;
                done += 1;
                self.publish(&rows);
                self.progress("classifying", done, Some(total), self.detail(phase, &rows))
                    .await;
                if self.ctx.canceled() {
                    canceled = true;
                    break;
                }
                match fail {
                    None if answered => any_ok = true,
                    None => {}
                    // 5xx / transport: the upstream or the model, not this one
                    // item. Anything else is this row's own problem.
                    Some(status) if !any_ok && status >= 500 => {
                        infra_errs += 1;
                        // The *first* error, which is the one that says what
                        // went wrong; the later ones are the same outage.
                        fatal_seen.get_or_insert_with(|| {
                            failed.unwrap_or_else(|| "model unavailable".into())
                        });
                        if infra_errs >= abort_after {
                            fatal = fatal_seen.clone();
                            // Dropping the stream cancels the doomed calls
                            // still in flight.
                            break;
                        }
                    }
                    Some(_) => {}
                }
            }
        }
        if let Some(why) = fatal {
            return Err((format!("model unavailable: {why}"), meter));
        }
        if let Some(why) = stopped {
            return Err((why, meter));
        }
        Ok((rows, meter, canceled))
    }

    /// One item's classify call: the rendered prompt, the output schema as
    /// `response_format`, the reply matched back onto the output (§4.2); or,
    /// with a question set, the set's question asked of the prompt
    /// ([`decide`]).
    async fn classify_one(&self, item: &BatchItem, mut row: Row, fetched: Value) -> Classified {
        let Some(out) = &item.output else {
            return Classified::left(row);
        };
        // A row with no id, or one whose fetch failed, is already an attention
        // row; §4.6 gives it the fallback rather than a model call.
        if row.id.is_empty() || row.error.is_some() {
            row.output = fallback_output(out);
            row.attention = true;
            return Classified::left(row);
        }

        let mut tctx = self.base.clone();
        tctx.item = row.item.clone();
        tctx.fetched = fetched;
        let user = template::render_text(item.user.as_deref().unwrap_or_default(), &tctx);
        // Through the set: its model's row is the run's, metered as a stamped
        // call is (the run's result reads `agent_meters`).
        if let Some(d) = &self.decider {
            let cancel = self.cancel();
            let done =
                decide::classify(&self.ctx.state, self.ctx.id, &cancel, d, out, row, user).await;
            return Classified {
                row: done.row,
                failed: done.failed,
                meter: Meter::default(),
                answered: done.answered,
                stop: done.stop,
            };
        }
        let Some(runner) = self.runner else {
            row.error = Some("no model was resolved for this run".to_string());
            row.output = fallback_output(out);
            row.attention = true;
            return Classified::left(row);
        };
        row.prompt = Some(user.clone());
        let mut messages: Vec<Message> = Vec::new();
        if let Some(s) = &item.system {
            let rendered = template::render_text(s, &tctx);
            if !rendered.trim().is_empty() {
                messages.push(Message::text(Role::System, rendered));
            }
        }
        messages.push(Message::text(Role::User, user));

        let schema = out.response_schema(&self.base.config);
        let mut ir = ChatRequest {
            model_alias: self.alias.clone(),
            messages,
            params: self.params(),
            tools: Vec::new(),
            tool_choice: None,
            stream: false,
            passthrough: Default::default(),
            response_format: None,
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
            context_management: None,
            compaction: None,
            thinking_display: None,
        };
        if !schema.is_null() {
            // Every egress translates it (structured-output design §6):
            // llama-server turns it into a grammar, Anthropic and Gemini
            // constrain their answer to it. The reply is still matched back
            // onto the enum below, for an answer that is not (§4.2).
            ir.response_format = Some(response_format(
                &schema,
                out.field.as_deref().unwrap_or("agent_output"),
                out.is_enum_form(),
            ));
        }

        // Raced against the cancel, not merely checked around: a classify call
        // is minutes of generation on a local reasoning model, and the run
        // between two rows is not where Cancel is pressed (§4.1). Abandoned
        // means the row stays exactly as it was listed — no error, no fallback,
        // no model call counted — and `classify_stage` stops on the next lap.
        // Metered like a tool loop's turn: its row's cost, however it ended,
        // and a model call once it was answered.
        let metered = MeteredRunner::new(runner);
        let Some(answer) = self
            .cancel()
            .guard(metered.run_turn(&ir, self.budget.wall_clock, &mut Discard))
            .await
        else {
            return Classified::left(row);
        };
        let meter = metered.tally();
        let answered = meter.model_calls > 0;
        let failed = match answer {
            Ok(completion) => {
                let reply = completion_text(&completion);
                let (value, is_fallback) = parse_reply(&reply, out, &self.base.config);
                // The whole reply, not a cut of it: the details modal shows what
                // the model actually said, and the table clips its cell in CSS.
                row.raw = Some(reply.trim().to_string());
                row.output = value;
                row.attention = is_fallback;
                None
            }
            Err(e) => {
                let status = e.http_status().as_u16();
                row.error = Some(e.to_string());
                row.output = fallback_output(out);
                row.attention = true;
                Some(status)
            }
        };
        Classified {
            row,
            failed,
            meter,
            answered,
            stop: None,
        }
    }

    /// Re-classify only a finished run's attention rows, against the *current*
    /// config (§2.4). Rows already classified keep their answer verbatim.
    async fn rerun(&mut self, input: &Input) -> Result<JobOutcome, String> {
        let RunSpec::Batch { item, .. } = self.agent.manifest.run.clone() else {
            return Err("this agent has no batch run".to_string());
        };
        if item.user.is_none() {
            return Err(
                "this agent declares no classify step (run.item.user), so there is nothing to \
                 re-run"
                    .to_string(),
            );
        }
        let base_job = input
            .base_job
            .ok_or("pass base_job: the finished run whose attention rows are re-classified")?;
        let mut rows = stored_rows(self.ctx, base_job, &self.agent.row.id).await?;
        if rows.is_empty() {
            return Err(format!("run #{base_job} has no rows to re-classify"));
        }
        let targets: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.attention && !r.id.is_empty())
            .map(|(i, _)| i)
            .collect();
        if targets.is_empty() {
            return Err(format!(
                "run #{base_job} has no rows needing attention; widen the config first, or start \
                 a fresh run"
            ));
        }
        self.publish(&rows);
        *self.overlay.lock().unwrap() = Some(rows.clone());

        // The attention rows are fetched again rather than carried over: the
        // fetch result is the email body, which is far too big to have kept in
        // the previous run's result column.
        let items: Vec<Value> = targets.iter().map(|&i| rows[i].item.clone()).collect();
        let (fresh, fetched, meter, canceled) =
            self.fetch_stage(Phase::Rerun, &item, items).await?;
        self.meter.absorb(meter);
        if canceled {
            return Ok(JobOutcome::CanceledWith(self.result(
                Phase::Rerun,
                &rows,
                None,
                true,
            )));
        }
        let pairs: Vec<(usize, Row, Value)> = fresh
            .into_iter()
            .zip(fetched)
            .enumerate()
            .map(|(i, (r, f))| (i, r, f))
            .collect();
        let staged = self.classify_stage(Phase::Rerun, &item, pairs).await;
        let (redone, canceled) = self.fold_stage(staged)?;
        for (slot, row) in targets.iter().zip(redone) {
            // A row the stage never reached (a stop, a cancel, the early
            // abort) keeps the base run's values, attention included.
            if row.asked() {
                rows[*slot] = row;
            }
        }
        *self.overlay.lock().unwrap() = None;
        self.publish(&rows);
        if canceled {
            return Ok(JobOutcome::CanceledWith(self.result(
                Phase::Rerun,
                &rows,
                None,
                true,
            )));
        }
        Ok(JobOutcome::Done(self.result(
            Phase::Rerun,
            &rows,
            None,
            false,
        )))
    }

    /// The only stage that writes (§2.4). Its own job, so a failure leaves the
    /// classify run and its rows untouched and Apply can simply be retried.
    async fn apply(&mut self, input: &Input) -> Result<JobOutcome, String> {
        let RunSpec::Batch { apply, .. } = self.agent.manifest.run.clone() else {
            return Err("this agent has no batch run".to_string());
        };
        let Some(step) = apply else {
            return Err(
                "this agent declares no apply step, so it has nothing to write".to_string(),
            );
        };
        if input.rows.is_empty() {
            return Err("nothing to apply: no rows were checked".to_string());
        }
        // Checked before apply, so a cancel that lands while the job is still
        // queued never writes anything at all (§4.1).
        if self.ctx.canceled() {
            return Ok(JobOutcome::Canceled);
        }
        let total = input.rows.len() as u64;
        // The rows being applied go into the live buffer before the step runs.
        // They are what the Run tab shows while an apply is in flight, and they
        // are where [`Run::failure_result`] reads them from if it breaks.
        self.publish(&input.rows);
        self.progress(
            "applying",
            0,
            Some(total),
            self.detail(Phase::Apply, &input.rows),
        )
        .await;

        let mut tctx = self.base.clone();
        tctx.rows = Value::Array(input.rows.iter().map(Row::for_apply).collect());
        // Done, cancelled or failed, the step reports what it called — one
        // place builds the `applied` block from it, and the failure path reads
        // that same block back out of `self.applied` (§4.1, §4.6).
        let (out, failed) = match self.run_step(&step, &tctx, "run.apply").await {
            Ok(out) => (out, None),
            Err(f) => (f.partial, Some(f.error)),
        };
        let applied = json!({
            "output": out.value,
            "text": out.text,
            "tool_calls": out.tool_calls,
        });
        self.meter.absorb(out.meter);
        *self.applied.lock().unwrap() = Some(applied.clone());
        if let Some(e) = failed {
            return Err(e);
        }
        self.progress(
            "applying",
            total,
            Some(total),
            self.detail(Phase::Apply, &input.rows),
        )
        .await;
        if self.ctx.canceled() {
            return Ok(JobOutcome::CanceledWith(self.result(
                Phase::Apply,
                &input.rows,
                Some(applied),
                true,
            )));
        }
        Ok(JobOutcome::Done(self.result(
            Phase::Apply,
            &input.rows,
            Some(applied),
            false,
        )))
    }

    /// The job's `result` column (§3, §4.5).
    fn result(&self, phase: Phase, rows: &[Row], applied: Option<Value>, canceled: bool) -> Value {
        // Anything that called in over HTTP stamped with `X-Lmgw-Run` belongs
        // to this run too (container-runtime §3.1) — one number, not two
        // halves, folded by the meter's own rule.
        let mut meter = self.meter.clone();
        meter.absorb(Meter::from(&self.ctx.state.agent_meters.read(self.ctx.id)));
        let mut out = meter.report();
        let m = out.as_object_mut().expect("report is an object");
        m.insert("phase".into(), json!(phase.as_str()));
        m.insert("agent_id".into(), json!(self.agent.row.id));
        m.insert("model".into(), json!(self.alias));
        m.insert("rows".into(), json!(rows));
        m.insert(
            "attention".into(),
            json!(rows.iter().filter(|r| r.attention).count()),
        );
        m.insert(
            "errors".into(),
            json!(rows.iter().filter(|r| r.error.is_some()).count()),
        );
        if canceled {
            m.insert("canceled".into(), json!(true));
        }
        if let Some(a) = applied {
            m.insert("applied".into(), a);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Reading a run back
// ---------------------------------------------------------------------------

/// The rows of a finished `agent_run` job of **this** agent, from its `result`
/// column. The key is checked, not just the kind: re-running another agent's
/// rows through this manifest would classify the wrong table.
async fn stored_rows(ctx: &JobCtx, job_id: i64, agent_id: &str) -> Result<Vec<Row>, String> {
    let key = super::job_key(agent_id);
    let row = store::get_job(&ctx.state.db, job_id)
        .await
        .map_err(|e| e.to_string())?
        .filter(|r| r.kind == super::JOB_KIND)
        .ok_or_else(|| format!("no agent run with job id {job_id}"))?;
    if row.key.as_deref() != Some(key.as_str()) {
        return Err(format!(
            "run #{job_id} is not a run of '{agent_id}'; re-run takes the base run of this agent"
        ));
    }
    let result: Value = row
        .result
        .as_deref()
        .and_then(|r| serde_json::from_str(r).ok())
        .unwrap_or(Value::Null);
    let rows = result.get("rows").cloned().unwrap_or(Value::Null);
    serde_json::from_value(rows)
        .map_err(|e| format!("run #{job_id} has no rows this build can read: {e}"))
}

/// One run's rows, wherever they currently live: the executor's live buffer
/// while it is in flight, the job's `result` once it has finished (§3).
pub fn rows_of(state: &SharedState, job_id: i64, result: Option<&Value>) -> Vec<Row> {
    if let Some(rows) = state.agent_runs.rows(job_id) {
        return rows;
    }
    result
        .and_then(|r| r.get("rows"))
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// The review table's columns, in the manifest's author order — `Row::columns`
/// is a map, and this build's maps are alphabetical (§2.6).
pub fn review_columns(m: &Manifest) -> Vec<String> {
    match &m.run {
        RunSpec::Batch { item, .. } => item.columns.keys().cloned().collect(),
        // A container declares its header order as a list, for the same reason
        // and with the same effect (container-runtime §4.1).
        RunSpec::Container { columns, .. } => columns.clone(),
        RunSpec::Chat { .. } => Vec::new(),
    }
}

/// The output fields the reviewer may override, each with the values it may be
/// set to — the enum built from the **stored** config, so widening the taxonomy
/// is a config edit and the review table picks it up (§2.4).
///
/// An empty option list means a free-form field: there is no enum to offer.
pub fn review_editable(m: &Manifest, config: &Value) -> Vec<(String, Vec<String>)> {
    match &m.run {
        RunSpec::Batch {
            item,
            review: Some(review),
            ..
        } => review
            .editable
            .iter()
            .map(|f| {
                let options = match &item.output {
                    Some(o) if o.field.as_deref() == Some(f.as_str()) => o.enum_values(config),
                    _ => Vec::new(),
                };
                (f.clone(), options)
            })
            .collect(),
        // A container's output shape is the image's business, so there is no
        // enum to offer: every editable field is free-form. Empty options is
        // exactly how the review table already spells that.
        RunSpec::Container {
            review: Some(review),
            ..
        } => review
            .editable
            .iter()
            .map(|f| (f.clone(), Vec::new()))
            .collect(),
        _ => Vec::new(),
    }
}

/// What a batch manifest can actually do, for the Run tab: a button for a stage
/// this agent does not declare would be a button that fails when pressed.
pub struct BatchShape {
    pub columns: Vec<String>,
    pub editable: Vec<(String, Vec<String>)>,
    pub has_classify: bool,
    pub has_apply: bool,
    /// The tools the apply step reaches, named on the Run tab so the reviewer
    /// knows what Apply is about to call.
    pub apply_tools: Vec<String>,
    /// `apply_tools` is a **ceiling**, not an itinerary: for a container the
    /// list is the union of the manifest's `tools[].allowed` — what the agent's
    /// token lets it reach — and the image decides which of them it calls. The
    /// Run tab says "may call" rather than "calls" when this is set
    /// (container-runtime §7).
    pub apply_tools_are_ceiling: bool,
}

impl BatchShape {
    pub fn of(m: &Manifest, config: &Value) -> Option<Self> {
        match &m.run {
            RunSpec::Batch { item, apply, .. } => Some(Self {
                columns: review_columns(m),
                editable: review_editable(m, config),
                has_classify: item.user.is_some() && item.output.is_some(),
                has_apply: apply.is_some(),
                apply_tools: match apply {
                    None => Vec::new(),
                    Some(step) => match (&step.tool, &step.turn) {
                        (Some(t), _) => vec![t.clone()],
                        (None, Some(turn)) => turn.tools.clone(),
                        // A script's calls are the script's; what bounds them
                        // is the token, so the Run tab prints the ceiling and
                        // says "may call" (container-runtime §7). Filled by the
                        // caller from `ToolSurface::allowed`.
                        (None, None) => Vec::new(),
                    },
                },
                apply_tools_are_ceiling: apply.as_ref().is_some_and(Step::is_script),
            }),
            // The container variant (container-runtime §7): no classify stage
            // — there is no per-item model call to make — and Apply exists
            // exactly when the image says it implements that phase.
            RunSpec::Container { phases, .. } => Some(Self {
                columns: review_columns(m),
                editable: review_editable(m, config),
                has_classify: false,
                has_apply: phases.iter().any(|p| p == "apply"),
                // Filled by the caller from `ToolSurface::allowed`, which is
                // the list `/mcp` actually filters on: `allowed: None` means
                // *the whole label*, so deriving it from `allowed` alone would
                // print an empty ceiling for the commonest manifest there is.
                apply_tools: Vec::new(),
                apply_tools_are_ceiling: true,
            }),
            RunSpec::Chat { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// A delta sink that keeps nothing: a classify call and the finalize call are
/// unary as far as the run is concerned, and the completion is the answer.
struct Discard;

impl DeltaSink for Discard {
    fn on_delta(&mut self, _: &crate::ir::StreamDelta) {}
}

fn shape_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The source result → the items to run over (§2.4).
fn items_from(src: &Value, items_path: Option<&str>) -> Result<Vec<Value>, String> {
    let arr = match items_path.filter(|p| !p.is_empty()) {
        // The result comes along: a tool that reports its own failure *in the
        // result* rather than through `isError` — which is what the Workspace
        // server does with an expired token or a quota, answering
        // `{"error": "…"}` — otherwise reduces to "which was an object", and
        // the one sentence that would have explained the run is dropped.
        Some(p) => src.pointer(p).cloned().ok_or_else(|| {
            format!(
                "run.items_path '{p}' names nothing in the source step's result, which was {}. \
                 The tool answered: {src}",
                shape_of(src)
            )
        })?,
        None => src.clone(),
    };
    match arr {
        Value::Array(items) => Ok(items),
        other => Err(format!(
            "the source step returned {}, not an array; a batch source must yield an array of \
             objects",
            shape_of(&other)
        )),
    }
}

/// The OpenAI `response_format` envelope around a bare JSON schema (§4.2).
///
/// `strict` is set **only for a schema this gateway generated** — the one-field
/// object [`ItemOutput::response_schema`] builds around the enum, which is
/// written to satisfy the mode. OpenAI's strict mode additionally requires
/// `additionalProperties: false` on every object and *every* property listed in
/// `required`; an author's `output` schema is their own text and is passed on
/// as written, so asking for strict on it would turn a manifest the gateway
/// accepted into a 400 from the upstream. Without the flag the schema is still
/// sent, still turned into a grammar by llama-server, and still checked here by
/// [`matches_schema`] when the answer comes back.
fn response_format(schema: &Value, name: &str, strict: bool) -> crate::ir::ResponseFormat {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    crate::ir::ResponseFormat::JsonSchema(crate::ir::JsonSchemaFormat {
        name: Some(if safe.is_empty() {
            "agent_output".to_string()
        } else {
            safe
        }),
        schema: Some(schema.clone()),
        strict: strict.then_some(true),
        ..Default::default()
    })
}

/// The turn's visible answer. Reasoning stands in when the model produced no
/// content at all, because "it only thought" is a diagnosable answer and an
/// empty string is not.
fn completion_text(c: &Completion) -> String {
    let text: String = c
        .content
        .iter()
        .filter_map(ContentPart::answer_text)
        .collect();
    if text.trim().is_empty() {
        c.reasoning.clone()
    } else {
        text
    }
}

/// The first JSON **object** in a reply: the whole string when it is one, else
/// the first balanced `{…}` inside it (a model that wrapped its answer in prose
/// or a code fence).
fn first_json_object(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(trimmed) {
        return Some(v);
    }
    let bytes = trimmed.as_bytes();
    let start = trimmed.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_str {
            match c {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return match serde_json::from_str::<Value>(&trimmed[start..=i]) {
                        Ok(v @ Value::Object(_)) => Some(v),
                        _ => None,
                    };
                }
            }
            _ => {}
        }
    }
    None
}

/// A structural check against the §2.2 output schema — the subset
/// `manifest::check_output_schema` already restricts a manifest to: an object,
/// its `required` names present, and each declared property's type and `enum`
/// honoured. Not a general JSON Schema validator, and it does not pretend to
/// be one: what it rejects is what the two-stage enforcement exists for.
pub(super) fn matches_schema(v: &Value, schema: &Value) -> Result<(), String> {
    let Value::Object(obj) = v else {
        return Err(format!("the answer is {}, not an object", shape_of(v)));
    };
    for name in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        match obj.get(name) {
            None | Some(Value::Null) => return Err(format!("'{name}' is missing")),
            Some(_) => {}
        }
    }
    let props = schema.get("properties").and_then(Value::as_object);
    for (name, spec) in props.into_iter().flatten() {
        let Some(got) = obj.get(name) else { continue };
        if let Some(want) = spec.get("type").and_then(Value::as_str) {
            let ok = match want {
                "string" => got.is_string(),
                "integer" => got.is_i64() || got.is_u64(),
                "number" => got.is_number(),
                "boolean" => got.is_boolean(),
                "array" => got.is_array(),
                "object" => got.is_object(),
                // A type this subset does not model is not a reason to refuse
                // an answer the author asked for.
                _ => true,
            };
            if !ok {
                return Err(format!(
                    "'{name}' is {}, and the schema asks for {want}",
                    shape_of(got)
                ));
            }
        }
        if let Some(values) = spec.get("enum").and_then(Value::as_array) {
            if !values.iter().any(|v| v == got) {
                return Err(format!(
                    "'{name}' is {got}, which is not one of the allowed values"
                ));
            }
        }
    }
    Ok(())
}

/// The answer a failed or unconvinced call gets, and the value that marks a row
/// as needing attention (§2.4).
fn fallback_output(out: &ItemOutput) -> Value {
    match (&out.field, &out.fallback) {
        (Some(f), Some(v)) => json!({ f.as_str(): v }),
        (Some(f), None) => json!({ f.as_str(): Value::Null }),
        (None, Some(v)) => v.clone(),
        (None, None) => Value::Null,
    }
}

/// A reply → the structured output, plus whether it landed on the fallback.
///
/// Where the route constrained the answer (a llama-server grammar, a provider's
/// structured output — structured-output design §6) the reply is exact and
/// the first branch takes it. A reply that is not — a model that answered
/// around the format — makes the enum form fall back to matching the text
/// ([`match_enum`]) and the schema form to the first JSON object in it, which
/// is the behaviour §4.2 asks for.
fn parse_reply(text: &str, out: &ItemOutput, config: &Value) -> (Value, bool) {
    if out.is_enum_form() {
        let field = out.field.clone().unwrap_or_else(|| "value".to_string());
        let values = out.enum_values(config);
        let fallback = out
            .fallback
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or_default();
        let structured = first_json_object(text)
            .and_then(|v| {
                v.get(&field)
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_string())
            })
            .and_then(|s| values.iter().find(|c| c.eq_ignore_ascii_case(&s)).cloned());
        let picked = structured.unwrap_or_else(|| match_enum(text, &values, fallback));
        let is_fallback = picked == fallback;
        return (json!({ field: picked }), is_fallback);
    }
    let Some(schema) = &out.schema else {
        return (Value::Null, true);
    };
    match first_json_object(text).filter(|v| matches_schema(v, schema).is_ok()) {
        Some(v) => (v, false),
        None => (fallback_output(out), true),
    }
}

/// The first position where `needle` appears in `hay` as a **whole word**:
/// neither neighbour is alphanumeric.
///
/// A plain substring search is what the mail workflow did, and it misfires on
/// short values — a category `Cat` matches inside "category", `IT` inside
/// "quit" — which tags a row with a taxonomy the model never chose. Both
/// strings arrive lowercased, so this is only about the boundaries.
fn word_at(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    hay.match_indices(needle)
        .find(|(pos, _)| {
            let before = hay[..*pos].chars().next_back();
            let after = hay[pos + needle.len()..].chars().next();
            before.is_none_or(|c| !c.is_alphanumeric())
                && after.is_none_or(|c| !c.is_alphanumeric())
        })
        .map(|(pos, _)| pos)
}

/// Map a free-text reply back onto one of the allowed values: first a line that
/// *is* one (which handles reason-then-answer replies), then the earliest one
/// mentioned **as a whole word**, else the fallback.
///
/// The fallback is excluded from the "mentioned anywhere" pass so a stray
/// mention of it cannot beat a real value that appears later in the sentence.
fn match_enum(raw: &str, values: &[String], fallback: &str) -> String {
    for line in raw.lines() {
        let cleaned = line.trim().trim_matches(|c: char| !c.is_alphanumeric());
        if cleaned.is_empty() {
            continue;
        }
        if let Some(v) = values.iter().find(|v| v.eq_ignore_ascii_case(cleaned)) {
            return v.clone();
        }
    }
    let low = raw.to_ascii_lowercase();
    let mut best: Option<(usize, &String)> = None;
    for v in values.iter().filter(|v| v.as_str() != fallback) {
        if let Some(pos) = word_at(&low, &v.to_ascii_lowercase()) {
            if best.is_none_or(|(p, _)| pos < p) {
                best = Some((pos, v));
            }
        }
    }
    match best {
        Some((_, v)) => v.clone(),
        None => fallback.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests (§8)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
