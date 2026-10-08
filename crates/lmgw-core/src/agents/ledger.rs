//! Layer 2 — the run ledger (container-runtime design §3.2).
//!
//! A caller lmgw did not start reports through a ledger of four event types.
//! The generic run surface — [`RunBuffer`](super::batch::RunBuffer) live rows,
//! the review table, cost per run, the jobs feed — renders them exactly as it
//! renders an in-process run, because they *become* [`Row`] values and a
//! [`JobProgress`].
//!
//! | type | fields | effect |
//! |---|---|---|
//! | `row` | `id` (required, non-empty), `columns?`, `output?`, `prompt?`, `raw?`, `error?`, `attention?` | upsert by `id`; fields absent from the event are left alone |
//! | `log` | `level`, `message` | one run-log line |
//! | `progress` | `done`, `total`, `stage` | one [`JobProgress`]; `detail` is filled by lmgw |
//! | `output` | `output` | the phase's terminal value; last one wins |
//!
//! Unknown `type` → a run-log line naming it, not a failure. Same for a `row`
//! carrying a column key the manifest did not declare: it is kept, appended to
//! the table after the declared ones in first-seen order, and noted once in the
//! run log. Surface it, never drop it silently.
//!
//! This module is transport A's decoder too — WP2's JSONL reader turns a line
//! into the same [`Event`] and calls the same [`Run::apply`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::jobs::{JobCtx, JobOutcome, JobProgress};

use super::batch::{Input, Phase, Row};

/// How often the wait loop looks at the cancel flag.
///
/// Cancellation is an `AtomicBool` with no waker (`jobs::JobCtx`), so the loop
/// has to poll it. Not a bound on anything the owner can observe: a `close`
/// wakes the loop at once through [`Run::wake`], and the deadline is honoured
/// to its own second regardless of this value.
const CANCEL_POLL: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// What one applied event did, for the run log and the `applied` count.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    /// The event changed the run.
    Yes,
    /// The event was understood and refused; the string is the run-log line.
    Rejected(String),
}

/// One decoded ledger event.
///
/// Decoded by hand rather than by `#[serde(tag = "type")]`, for two reasons
/// the derive cannot serve: an unknown `type` must become a run-log line
/// instead of an error, and a `row` event must be able to tell "the field was
/// absent" (leave it alone) from "the field was null".
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Row(Map<String, Value>),
    Log {
        level: String,
        message: String,
    },
    Progress(JobProgress),
    Output(Value),
    /// A `type` this build does not know, kept verbatim so the log line can
    /// name it.
    Unknown(String),
    /// Not an object at all, or no `type` key.
    Malformed(String),
}

impl Event {
    pub fn decode(v: &Value) -> Self {
        let Some(obj) = v.as_object() else {
            return Self::Malformed(format!("a ledger event must be an object, got {v}"));
        };
        let Some(ty) = obj.get("type").and_then(Value::as_str) else {
            return Self::Malformed(format!("a ledger event needs a \"type\"; got {v}"));
        };
        match ty {
            "row" => Self::Row(obj.clone()),
            "log" => Self::Log {
                level: obj
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("info")
                    .to_string(),
                // A non-string `message` is printed as it arrived rather than
                // dropped: a log line nobody can read still beats a log line
                // nobody gets.
                message: match obj.get("message") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                },
            },
            "progress" => Self::Progress(JobProgress {
                done: obj.get("done").and_then(Value::as_u64).unwrap_or(0),
                total: obj.get("total").and_then(Value::as_u64),
                stage: obj
                    .get("stage")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                detail: Value::Null,
            }),
            "output" => Self::Output(obj.get("output").cloned().unwrap_or(Value::Null)),
            other => Self::Unknown(other.to_string()),
        }
    }
}

/// Split a request body into events: one object, an array of them, or NDJSON.
///
/// A line that is not JSON is **not** dropped — it becomes a `Malformed` event,
/// which becomes a run-log line, exactly as a non-JSON stdout line does on
/// transport A.
pub fn decode_body(body: &str) -> Vec<Event> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(trimmed) {
        return items.iter().map(Event::decode).collect();
    }
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(trimmed) {
        return vec![Event::decode(&v)];
    }
    trimmed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|line| match serde_json::from_str::<Value>(line) {
            Ok(v) => Event::decode(&v),
            Err(_) => Event::Malformed(line.to_string()),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// How a ledger run ends
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseStatus {
    Done,
    Failed,
    Canceled,
}

impl CloseStatus {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "done" => Self::Done,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            _ => return None,
        })
    }
    pub fn names() -> &'static str {
        "done, failed, canceled"
    }
}

#[derive(Debug, Clone)]
pub struct Close {
    pub status: CloseStatus,
    pub detail: Option<String>,
    pub output: Option<Value>,
}

/// Why a ledger route refused. Each maps to one HTTP status and one stable
/// error `code` — the codes are the contract a container branches on, so they
/// are spelled here once rather than at each call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No bearer, or one that is not an agent token.
    TokenRequired,
    /// A valid agent token, but the run belongs to another agent.
    NotOwned,
    /// Cancel already ran; every later event and the close are refused.
    Cancelled,
    /// No job of this kind with this id, anywhere.
    NoRun(i64),
    /// Already closed by an earlier POST, or by the run ending.
    AlreadyClosed,
    /// The agent's own run, in flight — but lmgw is driving it, so there is no
    /// ledger to write to.
    NotLedger(i64),
    /// The token is this agent's and the agent is switched off. Disable is the
    /// kill switch (§3.1).
    AgentDisabled(String),
    /// The gateway could not answer — a read of the jobs table failed. Here
    /// rather than as a second error type so a route's refusal path is one
    /// small value, not a whole `Response` threaded through a `Result`.
    Unavailable(String),
}

impl Refusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::TokenRequired => "agent_token_required",
            Self::NotOwned => "run_not_owned",
            Self::Cancelled => "run_cancelled",
            Self::NoRun(_) => "not_found",
            Self::AlreadyClosed => "run_closed",
            Self::NotLedger(_) => "run_not_ledger",
            Self::AgentDisabled(_) => "agent_disabled",
            Self::Unavailable(_) => "op_failed",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::TokenRequired => 401,
            Self::NotOwned => 403,
            Self::Cancelled => 409,
            Self::NoRun(_) => 404,
            Self::AlreadyClosed | Self::NotLedger(_) => 409,
            Self::AgentDisabled(_) => 401,
            Self::Unavailable(_) => 500,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::TokenRequired => "this route needs the agent's own token as a bearer; \
                 get it from the agent page's Copy token, or agent_token_get"
                .to_string(),
            Self::NotOwned => {
                "that run belongs to a different agent than the token presented".to_string()
            }
            Self::Cancelled => {
                "this run was cancelled; nothing further is recorded against it".to_string()
            }
            Self::NoRun(id) => format!("no agent run with job id {id}"),
            Self::AlreadyClosed => "this run has already been closed".to_string(),
            Self::NotLedger(id) => format!(
                "run #{id} is in flight but lmgw is driving it, so it takes no ledger events"
            ),
            Self::AgentDisabled(agent) => format!(
                "agent '{agent}' is disabled; its token is refused until it is enabled again"
            ),
            Self::Unavailable(e) => e.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// One open run
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Inner {
    rows: Vec<Row>,
    /// Column keys the manifest did not declare, in first-seen order.
    extra_columns: Vec<String>,
    log: Vec<String>,
    output: Option<Value>,
    closed: Option<Close>,
    /// Progress events the executor has not reported yet.
    pending: Vec<JobProgress>,
}

/// A run opened through `POST /api/agents/{id}/runs`, alive until it is closed,
/// cancelled, or hits its deadline.
pub struct Run {
    pub agent_id: String,
    pub phase: Phase,
    /// The columns the manifest declares, so an undeclared one can be spotted.
    declared: Vec<String>,
    inner: Mutex<Inner>,
    /// The executor's cancel flag, installed when it picks the job up. Read by
    /// the routes so a `409 run_cancelled` lands on the very next POST rather
    /// than after the wait loop's next tick.
    cancel: Mutex<Option<Arc<AtomicBool>>>,
    wake: tokio::sync::Notify,
    opened_at: Instant,
    /// `0` = unbounded: the run ends when its opener says so. Surfaced in the
    /// open response and in the run log, never a silent bound.
    deadline_seconds: u64,
    /// This agent's token in plaintext, so [`Self::note`] can take it back out
    /// of anything the run writes to the log (§3.1, final review). `None` for
    /// an agent that has never minted one.
    secret: Option<String>,
}

impl Run {
    /// A run this desk does not hold.
    ///
    /// Public because transport A owns its accumulator rather than registering
    /// one: a container lmgw started reports on a pipe, and putting it on the
    /// [`Desk`] would open the HTTP ledger routes onto a run that already has a
    /// writer. Same type, same [`Self::apply`], same [`result_of`] — which is
    /// the point of there being one decoder (§3.2).
    pub fn new(
        agent_id: String,
        phase: Phase,
        declared: Vec<String>,
        deadline_seconds: u64,
        secret: Option<String>,
    ) -> Self {
        Self {
            agent_id,
            phase,
            declared,
            inner: Mutex::new(Inner::default()),
            cancel: Mutex::new(None),
            wake: tokio::sync::Notify::new(),
            opened_at: Instant::now(),
            deadline_seconds,
            secret,
        }
    }

    pub fn deadline_seconds(&self) -> u64 {
        self.deadline_seconds
    }

    /// Has Cancel been pressed? Either flag counts: the executor's, which
    /// `jobs::cancel` raises, or a `close` that said `canceled`.
    pub fn cancelled(&self) -> bool {
        if self
            .cancel
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
        {
            return true;
        }
        matches!(
            self.inner.lock().unwrap().closed,
            Some(Close {
                status: CloseStatus::Canceled,
                ..
            })
        )
    }

    fn deadline_passed(&self) -> bool {
        self.deadline_seconds > 0
            && self.opened_at.elapsed() >= Duration::from_secs(self.deadline_seconds)
    }

    /// One run-log line. Public because transport A writes the container's
    /// stderr into the same log, verbatim (§3.2) — verbatim *except* for the
    /// agent's own token, which [`Self::redact`] takes out on the way in, so a
    /// container that prints its `secrets.json` cannot publish its bearer
    /// through the run log, the stored `result` or the Run tab.
    pub fn note(&self, line: impl Into<String>) {
        let line = self.redact(line.into());
        self.inner.lock().unwrap().log.push(line);
    }

    /// This run's agent token, replaced by
    /// [`token::REDACTED`](super::token::REDACTED). Public because transport A
    /// mirrors each line into a second buffer and must push the same text.
    pub fn redact(&self, line: String) -> String {
        super::token::redact(self.secret.as_deref(), &line)
    }

    /// Seed the table with rows the opener handed over — an apply's reviewed
    /// rows, which both transports carry into the run.
    pub fn seed_rows(&self, rows: &[Row]) {
        self.inner.lock().unwrap().rows.extend_from_slice(rows);
    }

    /// The terminal value the run reported, if one arrived.
    pub fn output(&self) -> Option<Value> {
        self.inner.lock().unwrap().output.clone()
    }

    /// Apply one event. Returns what it did, so the caller can count and log.
    pub fn apply(&self, ev: &Event) -> Applied {
        match ev {
            Event::Log { level, message } => {
                self.note(format!("[{level}] {message}"));
                Applied::Yes
            }
            Event::Unknown(ty) => {
                let line = format!(
                    "ignored a ledger event of unknown type '{ty}' (known: row, log, progress, \
                     output)"
                );
                self.note(line.clone());
                Applied::Rejected(line)
            }
            Event::Malformed(raw) => {
                self.note(raw.clone());
                Applied::Rejected(raw.clone())
            }
            Event::Progress(p) => {
                self.inner.lock().unwrap().pending.push(p.clone());
                Applied::Yes
            }
            Event::Output(v) => {
                self.inner.lock().unwrap().output = Some(v.clone());
                Applied::Yes
            }
            Event::Row(obj) => self.apply_row(obj),
        }
    }

    fn apply_row(&self, obj: &Map<String, Value>) -> Applied {
        let id = obj.get("id").and_then(Value::as_str).unwrap_or_default();
        if id.is_empty() {
            // Catalog §2.3: no stable identity, nothing to apply against.
            let line = format!(
                "rejected a row event with no id: {}",
                Value::Object(obj.clone())
            );
            self.note(line.clone());
            return Applied::Rejected(line);
        }
        let mut undeclared: Vec<String> = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            let slot = match inner.rows.iter().position(|r| r.id == id) {
                Some(i) => i,
                None => {
                    inner.rows.push(Row {
                        id: id.to_string(),
                        ..Default::default()
                    });
                    inner.rows.len() - 1
                }
            };
            if let Some(Value::Object(cols)) = obj.get("columns") {
                for (k, v) in cols {
                    if !self.declared.iter().any(|d| d == k)
                        && !inner.extra_columns.iter().any(|d| d == k)
                    {
                        inner.extra_columns.push(k.clone());
                        undeclared.push(k.clone());
                    }
                    inner.rows[slot].columns.insert(k.clone(), v.clone());
                }
            }
            // Absent is "leave it alone", so a second row event may add an
            // output to a row that only had columns.
            if let Some(v) = obj.get("output") {
                inner.rows[slot].output = v.clone();
            }
            if let Some(v) = obj.get("prompt").and_then(Value::as_str) {
                inner.rows[slot].prompt = Some(v.to_string());
            }
            if let Some(v) = obj.get("raw").and_then(Value::as_str) {
                inner.rows[slot].raw = Some(v.to_string());
            }
            if let Some(v) = obj.get("error") {
                inner.rows[slot].error = match v {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    other => Some(other.to_string()),
                };
            }
            if let Some(v) = obj.get("attention").and_then(Value::as_bool) {
                inner.rows[slot].attention = v;
            }
        }
        for k in undeclared {
            self.note(format!(
                "row column '{k}' is not one the manifest declares; it is kept and shown after \
                 the declared columns"
            ));
        }
        Applied::Yes
    }

    /// The rows as they stand, for the live buffer the Run tab reads.
    pub fn rows(&self) -> Vec<Row> {
        self.inner.lock().unwrap().rows.clone()
    }

    pub fn log(&self) -> Vec<String> {
        self.inner.lock().unwrap().log.clone()
    }

    /// How many run-log lines there are right now.
    ///
    /// With [`Self::log_from`] this is how transport A mirrors the lines an
    /// *event* wrote — a `log` event, a rejected row, an undeclared column —
    /// into the live buffer without re-cloning the whole log per line.
    pub fn log_len(&self) -> usize {
        self.inner.lock().unwrap().log.len()
    }

    /// The run-log lines added since `from`.
    pub fn log_from(&self, from: usize) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        inner.log[from.min(inner.log.len())..].to_vec()
    }

    pub fn extra_columns(&self) -> Vec<String> {
        self.inner.lock().unwrap().extra_columns.clone()
    }

    /// Record the terminal value. Refused once the run is cancelled or closed.
    pub fn close(&self, close: Close) -> Result<(), Refusal> {
        if self.cancelled() {
            return Err(Refusal::Cancelled);
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.closed.is_some() {
            return Err(Refusal::AlreadyClosed);
        }
        if let Some(v) = &close.output {
            inner.output = Some(v.clone());
        }
        inner.closed = Some(close);
        drop(inner);
        self.wake.notify_waiters();
        Ok(())
    }

    /// The `progress` events nobody has reported yet. Public for the same
    /// reason [`Self::note`] is: transport A drains this loop too.
    pub fn take_pending(&self) -> Vec<JobProgress> {
        std::mem::take(&mut self.inner.lock().unwrap().pending)
    }
}

/// The phase's terminal value against the schema the run spec declares
/// (§3.2, WP1 amendment: "validated against the step's `output` schema at
/// close" — the schema lives on the container run spec, so WP1 stored it
/// unvalidated and this is where the check landed).
///
/// `None` for a schema nobody declared: a run that reports an output nothing
/// asked for still reports it. The check itself is
/// [`batch::matches_schema`](super::batch::matches_schema), the same
/// structural subset a `turn`'s output is held to — not a general JSON Schema
/// validator, and it does not pretend to be one.
pub fn check_output(
    schema: Option<&Value>,
    output: Option<&Value>,
    at: &str,
) -> Result<(), String> {
    let Some(schema) = schema else {
        return Ok(());
    };
    match output {
        None => Err(format!(
            "the run reported no output, and {at} declares a schema"
        )),
        Some(v) => super::batch::matches_schema(v, schema)
            .map_err(|e| format!("the run's output does not match {at}: {e}")),
    }
}

// ---------------------------------------------------------------------------
// The desk
// ---------------------------------------------------------------------------

/// Every ledger run currently open, keyed by job id. Lives on
/// [`AppState`](crate::state::AppState) for the same reason
/// [`RunBuffer`](super::batch::RunBuffer) does: two gateways in one test
/// process must not share runs through colliding job ids.
#[derive(Default)]
pub struct Desk(Mutex<HashMap<i64, Arc<Run>>>);

impl Desk {
    /// Create the entry, or hand back the one that is already there.
    ///
    /// Both the open route and the executor call this, and either may get there
    /// first: `jobs::spawn` returns the id *after* it has spawned the task, so
    /// the executor can be running before the route's next statement. Whoever
    /// arrives first creates it with the same values, because both read them
    /// from the same `Input` and the same manifest.
    #[allow(clippy::too_many_arguments)]
    pub fn attach(
        &self,
        job_id: i64,
        agent_id: &str,
        phase: Phase,
        declared: Vec<String>,
        deadline_seconds: u64,
        secret: Option<String>,
    ) -> Arc<Run> {
        self.0
            .lock()
            .unwrap()
            .entry(job_id)
            .or_insert_with(|| {
                Arc::new(Run::new(
                    agent_id.to_string(),
                    phase,
                    declared,
                    deadline_seconds,
                    secret,
                ))
            })
            .clone()
    }

    pub fn get(&self, job_id: i64) -> Option<Arc<Run>> {
        self.0.lock().unwrap().get(&job_id).cloned()
    }

    fn forget(&self, job_id: i64) {
        self.0.lock().unwrap().remove(&job_id);
    }
}

// ---------------------------------------------------------------------------
// The executor half
// ---------------------------------------------------------------------------

/// The `deadline_seconds` this manifest asks for. Declared, defaulted and
/// printed — never a bound lmgw invented on the caller's behalf.
pub fn deadline_seconds(m: &super::Manifest) -> u64 {
    m.limits().deadline_seconds
}

/// Drive one ledger-opened run: wait for events, report progress, end the job
/// the way the closing POST said to.
///
/// The deadline runs from the **open**, not from a container start — there may
/// be no container. A run that reaches it with no `close` ends `failed` with
/// reason `no_close`: the honest reading of "the process that opened this is
/// gone", and the reason a ledger run cannot leak a live job row forever.
pub async fn execute(ctx: &JobCtx, input: &Input) -> Result<JobOutcome, String> {
    let out = open_and_wait(ctx, input).await;
    if out.is_err() {
        // The open route registers the desk entry too, because a container can
        // POST an event before this task is scheduled. If the run never got
        // going, that entry has nobody to close it.
        ctx.state.agent_ledger.forget(ctx.id);
    }
    out
}

async fn open_and_wait(ctx: &JobCtx, input: &Input) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    let row = crate::store::get_agent(&state.db, &input.agent_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no agent with id '{}'", input.agent_id))?;
    let mut agent = super::Agent::from_row(row)?;
    // A ledger run is opened from outside rather than from the Run tab, so
    // this is almost always empty — applied all the same, so the two run
    // shapes read one config the same way.
    agent.override_config(&input.values)?;
    if !agent.row.enabled {
        return Err(format!(
            "agent '{}' is disabled; enable it on the catalog before running it",
            input.agent_id
        ));
    }
    let run = state.agent_ledger.attach(
        ctx.id,
        &input.agent_id,
        input.phase,
        super::batch::review_columns(&agent.manifest),
        deadline_seconds(&agent.manifest),
        super::token::plaintext_of(&state.snapshot(), &input.agent_id),
    );
    *run.cancel.lock().unwrap() = Some(ctx.cancel_signal());

    // The rows the opener handed over (an apply's reviewed table) start the
    // ledger off, so `{{rows}}`-shaped work has something to report against.
    run.seed_rows(&input.rows);
    run.note(match run.deadline_seconds {
        0 => "run opened through the ledger; no deadline (run.limits.deadline_seconds = 0)".into(),
        n => format!("run opened through the ledger; deadline in {n}s"),
    });
    publish(ctx, &run);

    let outcome = wait(ctx, &run).await;

    let totals = state.agent_meters.take(ctx.id);
    let rows = run.rows();
    let mut result = result_of(&run, input.phase, &input.agent_id, &rows, &totals);
    if let Some(m) = result.as_object_mut() {
        m.insert("ledger".into(), json!(true));
    }
    state.agent_ledger.forget(ctx.id);
    // The declared `output` schema is checked at close, on both transports
    // (§3.2, WP1 amendment): a run that closes `done` having reported the wrong
    // shape must not store it as though it were right.
    let at = format!("run.output.{}", input.phase.as_str());
    let schema = agent.manifest.output_schema(input.phase.as_str());
    Ok(match outcome {
        Ending::Closed(Close {
            status: CloseStatus::Done,
            ..
        }) => match check_output(schema, run.output().as_ref(), &at) {
            Ok(()) => JobOutcome::Done(result),
            Err(error) => JobOutcome::FailedWith {
                error,
                value: result,
            },
        },
        Ending::Closed(Close {
            status: CloseStatus::Failed,
            detail,
            ..
        }) => JobOutcome::FailedWith {
            error: detail.unwrap_or_else(|| "the run reported failed with no detail".into()),
            value: result,
        },
        Ending::Closed(Close {
            status: CloseStatus::Canceled,
            ..
        })
        | Ending::Cancelled => JobOutcome::CanceledWith(result),
        Ending::NoClose(n) => JobOutcome::FailedWith {
            error: format!(
                "no_close: the run reached its deadline of {n}s without a close \
                 (POST /api/agents/runs/{}/close)",
                ctx.id
            ),
            value: result,
        },
    })
}

enum Ending {
    Closed(Close),
    Cancelled,
    NoClose(u64),
}

async fn wait(ctx: &JobCtx, run: &Arc<Run>) -> Ending {
    // How much of the run log the dashboard has already been told about. A
    // `log` event moves it, and moving it is what makes the jobs frame change
    // for a run that has produced no row yet (§3.2, final review).
    let mut seen_log = run.log_len();
    loop {
        // A close wakes this at once; the tick is only there because the
        // cancel flag has no waker.
        let _ = tokio::time::timeout(CANCEL_POLL, run.wake.notified()).await;
        drain_progress(ctx, run, &mut seen_log).await;
        if let Some(close) = run.inner.lock().unwrap().closed.clone() {
            return Ending::Closed(close);
        }
        if ctx.canceled() {
            return Ending::Cancelled;
        }
        if run.deadline_passed() {
            run.note(format!(
                "no_close: the deadline of {}s passed with the run still open",
                run.deadline_seconds
            ));
            return Ending::NoClose(run.deadline_seconds);
        }
    }
}

/// Seed the buffer the Run tab reads with whatever the opener handed over, so a
/// reader sees the run's starting table before the first event arrives. The
/// events route keeps it current from then on, which is why the wait loop only
/// drains progress.
fn publish(ctx: &JobCtx, run: &Arc<Run>) {
    ctx.state.agent_runs.put(ctx.id, &run.rows());
}

/// Report whatever `progress` events have arrived since the last look, **or**
/// the fact that the run log grew. Nothing happens when neither did — the
/// loop's tick exists for the cancel flag, not to re-publish a table that did
/// not change.
///
/// The log half matters on its own: a run that only emits `log` events (a
/// container's startup chatter, a pull, diagnostics) changes no counter and no
/// row, so without it the frame stayed identical and the Run tab had no reason
/// to re-read — empty for precisely the phase the owner is watching.
async fn drain_progress(ctx: &JobCtx, run: &Arc<Run>, seen_log: &mut usize) {
    let pending = run.take_pending();
    if pending.is_empty() {
        let now = run.log_len();
        if now != *seen_log {
            *seen_log = now;
            ctx.progress_detail(detail_of(run)).await;
        }
        return;
    }
    *seen_log = run.log_len();
    let detail = detail_of(run);
    for mut p in pending {
        p.detail = detail.clone();
        ctx.progress(p).await;
    }
}

/// `{agent_id, phase, attention, errors, log_lines}` — counts and nothing more,
/// exactly as an in-process run's `Run::detail` produces, plus the one number
/// that lets a reader notice a log-only run.
///
/// `log_lines` is **monotonic** and is the whole push half of the Run tab: the
/// dashboard re-reads `AgentRunDetail` when the frame changes, and this is the
/// field that changes when nothing else does. A count, not the lines: fifty
/// lines of container stdout on every frame to every open tab is the wrong pipe
/// (§3), and the endpoint that already serves the log is the right one.
pub fn detail_of(run: &Run) -> Value {
    // One lock, no clone: this is also what a stderr line pays for, and a
    // chatty container prints thousands of them.
    let inner = run.inner.lock().unwrap();
    json!({
        "agent_id": run.agent_id,
        "phase": run.phase.as_str(),
        "attention": inner.rows.iter().filter(|r| r.attention).count(),
        "errors": inner.rows.iter().filter(|r| r.error.is_some()).count(),
        "log_lines": inner.log.len(),
    })
}

/// The job's `result` column, in the same vocabulary `batch::Run::result`
/// writes so the Runs tab, the cost line and the review table read one shape.
///
/// Public and transport-agnostic: the JSONL runner (§3.2, transport A) builds
/// its result from the same function, so a container run and a hand-posted one
/// are indistinguishable to everything downstream — which is the property the
/// whole ledger design rests on. Each transport adds its own marker key
/// afterwards (`ledger` / `container`).
pub fn result_of(
    run: &Run,
    phase: Phase,
    agent_id: &str,
    rows: &[Row],
    totals: &super::RunTotals,
) -> Value {
    let inner = run.inner.lock().unwrap();
    let mut out = json!({
        "usage": totals.usage,
        // NULL, never 0, when nothing could be priced: "we do not know" and
        // "it was free" are different answers.
        "cost_micro": totals.cost_micro(),
        "model_calls": totals.model_calls,
        "tool_calls": totals.tool_calls,
        "phase": phase.as_str(),
        "agent_id": agent_id,
        "rows": rows,
        "attention": rows.iter().filter(|r| r.attention).count(),
        "errors": rows.iter().filter(|r| r.error.is_some()).count(),
        "log": inner.log,
    });
    let m = out.as_object_mut().expect("an object");
    if let Some(v) = &inner.output {
        m.insert("output".into(), v.clone());
    }
    if !inner.extra_columns.is_empty() {
        m.insert("extra_columns".into(), json!(inner.extra_columns));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_of(declared: &[&str]) -> Arc<Run> {
        Arc::new(Run::new(
            "a".into(),
            Phase::Run,
            declared.iter().map(|s| (*s).to_string()).collect(),
            0,
            None,
        ))
    }

    fn apply_all(run: &Arc<Run>, body: &str) -> Vec<Applied> {
        decode_body(body).iter().map(|e| run.apply(e)).collect()
    }

    /// The push half of the Run tab (§3.2, final review): the dashboard
    /// re-reads the run when the jobs frame changes, and a run that emits only
    /// `log` events changes no counter and no row. `log_lines` is the field
    /// that moves, and it only ever goes up.
    #[test]
    fn a_log_event_bumps_the_log_line_count_the_jobs_frame_carries() {
        let run = run_of(&["subject"]);
        let start = detail_of(&run)["log_lines"].as_u64().unwrap();

        apply_all(
            &run,
            r#"{"type":"log","level":"info","message":"pulling the image"}"#,
        );
        let after_one = detail_of(&run)["log_lines"].as_u64().unwrap();
        assert_eq!(after_one, start + 1, "a log event has to move the frame");

        apply_all(&run, r#"{"type":"log","message":"still pulling"}"#);
        assert_eq!(detail_of(&run)["log_lines"].as_u64().unwrap(), start + 2);

        // Monotonic, and the counts it travels with are unaffected: the frame
        // says "there is more log", not "the table changed".
        let d = detail_of(&run);
        assert_eq!(d["attention"], json!(0), "{d}");
        assert_eq!(d["errors"], json!(0), "{d}");

        // Not only `log` events: anything that writes a run-log line does it,
        // which is what makes a container's stderr and a rejected event visible
        // for the same reason.
        apply_all(&run, r#"{"type":"banner","message":"hi"}"#);
        let after_unknown = detail_of(&run)["log_lines"].as_u64().unwrap();
        assert!(
            after_unknown > start + 2,
            "a rejected event is a log line too: {after_unknown}"
        );
        run.note("the container's stderr, verbatim");
        assert_eq!(
            detail_of(&run)["log_lines"].as_u64().unwrap(),
            after_unknown + 1
        );

        // And a row event alone does not touch it.
        let before_row = detail_of(&run)["log_lines"].as_u64().unwrap();
        apply_all(
            &run,
            r#"{"type":"row","id":"m1","columns":{"subject":"Invoice"}}"#,
        );
        assert_eq!(
            detail_of(&run)["log_lines"].as_u64().unwrap(),
            before_row,
            "a row is not a log line"
        );
    }

    /// The agent's own token never reaches the run log, however it got into the
    /// line (§3.1, final review) — a container that prints its `secrets.json`
    /// is the case this exists for.
    #[test]
    fn the_agents_token_is_redacted_out_of_every_run_log_line() {
        let secret = "lmgw-agent-deadbeef";
        let run = Arc::new(Run::new(
            "a".into(),
            Phase::Run,
            vec!["subject".to_string()],
            0,
            Some(secret.to_string()),
        ));
        apply_all(
            &run,
            &format!(r#"{{"type":"log","message":"token is {secret} here"}}"#),
        );
        run.note(format!("{{\"token\":\"{secret}\"}}"));
        let text = result_of(
            &run,
            Phase::Run,
            "a",
            &run.rows(),
            &crate::agents::RunTotals::default(),
        )
        .to_string();
        assert!(
            !text.contains(secret),
            "the token survived into the log: {text}"
        );
        assert!(text.contains("<agent token>"), "{text}");
    }

    #[test]
    fn a_second_row_event_adds_to_the_row_the_first_one_made() {
        let run = run_of(&["from"]);
        apply_all(&run, r#"{"type":"row","id":"m1","columns":{"from":"a@b"}}"#);
        apply_all(
            &run,
            r#"{"type":"row","id":"m1","output":{"category":"news"}}"#,
        );
        let rows = run.rows();
        assert_eq!(rows.len(), 1, "upserted by id, not appended");
        assert_eq!(rows[0].columns["from"], json!("a@b"), "kept");
        assert_eq!(rows[0].output, json!({"category":"news"}), "added");
    }

    #[test]
    fn a_row_with_no_id_is_rejected_and_named_in_the_log() {
        let run = run_of(&[]);
        let applied = apply_all(&run, r#"{"type":"row","columns":{"x":1}}"#);
        assert!(matches!(applied[0], Applied::Rejected(_)));
        assert!(run.rows().is_empty());
        assert!(run.log()[0].contains("no id"), "{:?}", run.log());
    }

    #[test]
    fn an_unknown_type_is_a_log_line_not_a_failure() {
        let run = run_of(&[]);
        let applied = apply_all(&run, r#"{"type":"banner","message":"hi"}"#);
        assert!(matches!(applied[0], Applied::Rejected(_)));
        assert!(run.log()[0].contains("'banner'"), "{:?}", run.log());
    }

    #[test]
    fn an_undeclared_column_is_kept_and_noted_once() {
        let run = run_of(&["from"]);
        apply_all(
            &run,
            "{\"type\":\"row\",\"id\":\"m1\",\"columns\":{\"from\":\"a\",\"spam\":1}}\n\
             {\"type\":\"row\",\"id\":\"m2\",\"columns\":{\"spam\":2}}",
        );
        assert_eq!(run.extra_columns(), vec!["spam".to_string()]);
        assert_eq!(run.rows()[1].columns["spam"], json!(2), "never dropped");
        assert_eq!(
            run.log().iter().filter(|l| l.contains("'spam'")).count(),
            1,
            "noted once, not once per row"
        );
    }

    #[test]
    fn a_non_json_line_becomes_a_log_line_verbatim() {
        let run = run_of(&[]);
        let applied = apply_all(
            &run,
            "Listening on :8080\n{\"type\":\"log\",\"message\":\"go\"}",
        );
        assert!(matches!(applied[0], Applied::Rejected(_)));
        assert_eq!(applied[1], Applied::Yes);
        assert_eq!(run.log()[0], "Listening on :8080");
        assert_eq!(run.log()[1], "[info] go");
    }

    #[test]
    fn a_json_array_and_ndjson_decode_to_the_same_events() {
        let array = decode_body(r#"[{"type":"log","message":"a"},{"type":"log","message":"b"}]"#);
        let ndjson = decode_body(
            "{\"type\":\"log\",\"message\":\"a\"}\n{\"type\":\"log\",\"message\":\"b\"}",
        );
        assert_eq!(array, ndjson);
        assert_eq!(array.len(), 2);
    }

    #[test]
    fn progress_and_output_land_where_the_run_surface_reads_them() {
        let run = run_of(&[]);
        apply_all(
            &run,
            r#"[{"type":"progress","done":3,"total":10,"stage":"labelling"},
                {"type":"output","output":{"applied":3}}]"#,
        );
        let pending = run.take_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].done, 3);
        assert_eq!(pending[0].total, Some(10));
        assert_eq!(pending[0].stage, "labelling");
        assert_eq!(run.inner.lock().unwrap().output, Some(json!({"applied":3})));
    }

    #[test]
    fn a_cancelled_run_refuses_its_close() {
        let run = run_of(&[]);
        let flag = Arc::new(AtomicBool::new(true));
        *run.cancel.lock().unwrap() = Some(flag);
        assert_eq!(
            run.close(Close {
                status: CloseStatus::Done,
                detail: None,
                output: None,
            })
            .unwrap_err(),
            Refusal::Cancelled
        );
    }

    #[test]
    fn closing_twice_is_refused_rather_than_silently_overwriting() {
        let run = run_of(&[]);
        let close = || Close {
            status: CloseStatus::Done,
            detail: None,
            output: None,
        };
        run.close(close()).unwrap();
        assert_eq!(run.close(close()).unwrap_err(), Refusal::AlreadyClosed);
    }
}
