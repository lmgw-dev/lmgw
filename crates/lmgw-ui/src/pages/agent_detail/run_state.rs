use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{AgentBatchShape, AgentRunDetail, AgentRunSummary};
use serde_json::{json, Value};

use crate::scope::Scope;

// ---------------------------------------------------------------------------
// The run surface's state (§6.2)
// ---------------------------------------------------------------------------

/// One row of the review table, as the page holds it.
///
/// The server's row travels along unchanged in `raw`: Apply posts it back with
/// only the reviewer's overrides patched in, so a column or an output field
/// this build does not render is still carried to the apply step rather than
/// being quietly dropped on the way through the browser.
#[derive(Clone, PartialEq)]
pub struct ReviewRow {
    pub id: String,
    /// `(name, text)` in the manifest's column order, which the server sends
    /// separately — a JSON object arrives alphabetized.
    pub cells: Vec<(String, String)>,
    pub error: Option<String>,
    pub raw: Option<String>,
    /// The exact `user` text the model was given, for the details modal.
    pub prompt: Option<String>,
    pub attention: bool,
    /// The model answered for this row. False for a list-only run and for a
    /// row a run in flight has not reached yet.
    pub answered: bool,
    /// The **server's** value for each editable output field, in shape order.
    /// What the reviewer changed it to is not here: that lives on [`RunState`]
    /// and outlives this struct, which is rebuilt from the server on every
    /// progress frame.
    pub answers: Vec<(String, String)>,
    raw_row: Value,
}

impl ReviewRow {
    /// The identity of the **rendered** row.
    ///
    /// A keyed `<For>` builds a view only for a key it has not seen, so keying
    /// on the row id alone freezes a row's cells at whatever they were when it
    /// first appeared — and a batch run publishes every row before it has
    /// classified any of them, so that is "no answer, ever". The key therefore
    /// carries everything the row *displays*. Re-rendering costs nothing,
    /// because the reviewer's own state is not in here.
    pub fn view_key(&self) -> String {
        let mut k = String::with_capacity(64);
        k.push_str(&self.id);
        for (_, text) in &self.cells {
            k.push('\u{1}');
            k.push_str(text);
        }
        for (_, value) in &self.answers {
            k.push('\u{1}');
            k.push_str(value);
        }
        k.push('\u{1}');
        k.push_str(self.raw.as_deref().unwrap_or(""));
        k.push('\u{1}');
        k.push_str(self.error.as_deref().unwrap_or(""));
        k.push('\u{1}');
        k.push(if self.attention { 'a' } else { '-' });
        k.push(if self.answered { 'y' } else { '-' });
        k
    }

    /// The row as Apply receives it: the server's own row with the reviewer's
    /// overrides written into `output`.
    pub(super) fn for_apply(&self, overrides: &Overrides) -> Value {
        let mine: Vec<(String, String)> = self
            .answers
            .iter()
            .filter_map(|(field, _)| {
                overrides
                    .get(&(self.id.clone(), field.clone()))
                    .map(|v| (field.clone(), v.clone()))
            })
            .collect();
        patched_row(&self.raw_row, &mine)
    }
}

/// `(row id, output field)` → what the reviewer set it to. Keyed by row id
/// rather than held per row, because the row list is replaced wholesale every
/// time the run publishes a frame.
pub type Overrides = std::collections::HashMap<(String, String), String>;

/// How many decisions the reviewer made that the run did not come back with:
/// each unticked row, and each answer changed to something other than the
/// model's. An answer set back to what the model said is no edit.
pub fn review_edits(
    rows: &[ReviewRow],
    unchecked: &std::collections::HashSet<String>,
    overrides: &Overrides,
) -> usize {
    let changed = overrides
        .iter()
        .filter(|((id, field), value)| {
            let server = rows
                .iter()
                .find(|r| r.id == *id)
                .and_then(|r| r.answers.iter().find(|(f, _)| f == field))
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            server != value.as_str()
        })
        .count();
    unchecked.len() + changed
}

/// One row's review columns, in the manifest's order (the server sends the
/// order separately, because a JSON object arrives alphabetized here).
pub fn row_cells(row: &Value, columns: &[String]) -> Vec<(String, String)> {
    columns
        .iter()
        .map(|c| {
            let text = match &row["columns"][c] {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            (c.clone(), text)
        })
        .collect()
}

/// The server's row with the reviewer's overrides written into `output`.
///
/// Everything else travels back untouched — a column or an output field this
/// build does not render is still carried to the apply step rather than being
/// quietly dropped on the way through the browser.
pub fn patched_row(raw: &Value, overrides: &[(String, String)]) -> Value {
    let mut v = raw.clone();
    if overrides.is_empty() {
        return v;
    }
    if let Some(obj) = v.as_object_mut() {
        let slot = obj.entry("output").or_insert_with(|| json!({}));
        if !slot.is_object() {
            *slot = json!({});
        }
        if let Some(out) = slot.as_object_mut() {
            for (field, value) in overrides {
                out.insert(field.clone(), Value::String(value.clone()));
            }
        }
    }
    v
}

/// Build the table from one run's rows and the agent's review shape.
pub fn review_rows(rows: &[Value], shape: &AgentBatchShape) -> Vec<ReviewRow> {
    rows.iter()
        .map(|row| ReviewRow {
            id: row["id"].as_str().unwrap_or_default().to_string(),
            cells: row_cells(row, &shape.columns),
            error: row["error"].as_str().map(str::to_string),
            raw: row["raw"].as_str().map(str::to_string),
            prompt: row["prompt"].as_str().map(str::to_string),
            attention: row["attention"].as_bool().unwrap_or(false),
            answered: match &row["output"] {
                Value::Object(o) => !o.is_empty(),
                Value::Null => false,
                _ => true,
            },
            answers: shape
                .editable
                .iter()
                .map(|f| {
                    let current = row["output"][&f.field]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    (f.field.clone(), current)
                })
                .collect(),
            raw_row: row.clone(),
        })
        .collect()
}

/// What the page watches on the jobs feed for "this agent's run":
/// `(job id, done, status, log lines)`. The fourth field is why it is a tuple
/// worth naming — a container run can move only that one for minutes.
pub(super) type Tick = (i64, u64, String, Option<u64>);

/// The whole batch run surface, as one `Copy` bundle the page owns.
#[derive(Clone, Copy)]
pub struct RunState {
    /// The run on screen: live, just finished, or reopened from Runs.
    pub summary: RwSignal<Option<AgentRunSummary>>,
    pub rows: RwSignal<Vec<ReviewRow>>,
    /// The finished job's `result`: the usage total, and Apply's output.
    pub result: RwSignal<Option<Value>>,
    /// The run log (container-runtime §3.2): `log` events, the lines lmgw wrote
    /// about events it rejected, and a container's non-JSON stdout. Live from
    /// the ledger while the run is open, from the stored result after — one
    /// field either way, because the endpoint answers both.
    pub log: RwSignal<Vec<String>>,
    pub shape: RwSignal<Option<AgentBatchShape>>,
    pub busy: RwSignal<bool>,
    pub error: RwSignal<Option<String>>,
    pub modal_open: RwSignal<bool>,
    pub modal_title: RwSignal<String>,
    pub modal_body: RwSignal<String>,
    pub confirm_apply: RwSignal<bool>,
    /// Row ids the reviewer has **un**ticked. The exception is stored, not the
    /// rule, because a row arrives checked; and it is stored here rather than
    /// in the row, because [`Self::rows`] is replaced from the server on every
    /// progress frame — a signal inside a row would be swapped out from under
    /// the checkbox the reviewer just clicked (§6.2).
    pub unchecked: RwSignal<std::collections::HashSet<String>>,
    /// The reviewer's edits, keyed by row, for the same reason.
    pub overrides: RwSignal<Overrides>,
    /// A run the owner deliberately reopened from the Runs tab. While it is
    /// set, the jobs frame does not pull the surface back to whatever is live:
    /// "show me run #7" has to survive run #9 starting.
    pub pinned: RwSignal<Option<i64>>,
    /// A `GET /api/agents/runs/<id>` is out. See [`Self::load`].
    fetching: RwSignal<bool>,
    /// The run a frame asked for while one was already out, to be read as soon
    /// as it lands. Backpressure, not a cap: nothing is dropped, the newest ask
    /// simply wins.
    queued: RwSignal<Option<i64>>,
    /// The page's lifetime: a run read that lands after the page was left is
    /// dropped (review code:C4).
    pub scope: Scope,
}

impl Default for RunState {
    fn default() -> Self {
        Self::new()
    }
}

impl RunState {
    pub fn new() -> Self {
        Self {
            summary: RwSignal::new(None),
            rows: RwSignal::new(Vec::new()),
            result: RwSignal::new(None),
            log: RwSignal::new(Vec::new()),
            shape: RwSignal::new(None),
            busy: RwSignal::new(false),
            error: RwSignal::new(None),
            modal_open: RwSignal::new(false),
            modal_title: RwSignal::new(String::new()),
            modal_body: RwSignal::new(String::new()),
            confirm_apply: RwSignal::new(false),
            unchecked: RwSignal::new(Default::default()),
            overrides: RwSignal::new(Default::default()),
            pinned: RwSignal::new(None),
            fetching: RwSignal::new(false),
            queued: RwSignal::new(None),
            scope: Scope::new(),
        }
    }

    /// Is this row going to Apply? Everything is, until it is unticked.
    pub fn is_checked(&self, id: &str) -> bool {
        !self.unchecked.get().contains(id)
    }

    pub fn set_checked(&self, id: &str, on: bool) {
        self.unchecked.update(|set| {
            if on {
                set.remove(id);
            } else {
                set.insert(id.to_string());
            }
        });
    }

    /// The reviewer's value for one field, or the server's if untouched.
    pub fn value_of(&self, id: &str, field: &str, server: &str) -> String {
        self.overrides
            .get()
            .get(&(id.to_string(), field.to_string()))
            .cloned()
            .unwrap_or_else(|| server.to_string())
    }

    /// [`review_edits`] of the run on screen, tracked.
    pub fn edits(&self) -> usize {
        self.rows.with(|rs| {
            self.unchecked
                .with(|u| self.overrides.with(|o| review_edits(rs, u, o)))
        })
    }

    pub fn set_override(&self, id: &str, field: &str, value: String) {
        self.overrides.update(|m| {
            m.insert((id.to_string(), field.to_string()), value);
        });
    }

    /// Everything the reviewer decided about the run that is on screen. Dropped
    /// when a different run takes its place — a tick against run #7 means
    /// nothing to run #9.
    fn forget_review(&self) {
        self.unchecked.update(std::collections::HashSet::clear);
        self.overrides.update(Overrides::clear);
    }

    pub fn job_id(&self) -> Option<i64> {
        self.summary.get().map(|s| s.job_id)
    }

    pub(super) fn clear(&self) {
        // A queued read belongs to the run that is being cleared away.
        self.queued.set(None);
        self.summary.set(None);
        self.rows.set(Vec::new());
        self.result.set(None);
        self.log.set(Vec::new());
        self.shape.set(None);
        self.error.set(None);
        self.pinned.set(None);
        self.forget_review();
    }

    /// Read one run: its rows come from the executor's live buffer while it is
    /// in flight and from the job's result after, and this endpoint answers
    /// both (§3), so the page never has to know which.
    ///
    /// **One fetch in flight at a time.** The jobs frame now also changes on a
    /// log line, so a container that talks fires this several times a second;
    /// stacking the GETs would let a slow answer arrive after a newer one and
    /// paint a stale table. An ask that arrives while one is out is remembered
    /// and served the moment it lands — the request rate follows the server's
    /// own response time, which is backpressure rather than an invented
    /// interval, and nothing is silently dropped.
    pub fn load(&self, job_id: i64) {
        let me = *self;
        if me.fetching.get_untracked() {
            me.queued.set(Some(job_id));
            return;
        }
        me.fetching.set(true);
        // A *different* run replacing the one on screen takes the review with
        // it. The same run re-read — which is what a progress frame does,
        // several times a second — must not, or the table would forget every
        // tick and every edit while it is still filling up.
        if me.summary.get_untracked().map(|s| s.job_id) != Some(job_id) {
            me.forget_review();
        }
        me.scope.spawn(async move {
            match crate::api::get::<AgentRunDetail>(&format!("/api/agents/runs/{job_id}")).await {
                Ok(d) => {
                    let shape = d.batch.clone().unwrap_or_default();
                    me.rows.set(review_rows(&d.rows, &shape));
                    me.shape.set(d.batch);
                    me.result.set(d.result);
                    me.log.set(d.log);
                    me.summary.set(Some(d.job));
                    me.error.set(None);
                }
                Err(e) => me.error.set(Some(e.to_string())),
            }
            me.fetching.set(false);
            if let Some(next) = me.queued.get_untracked() {
                me.queued.set(None);
                me.load(next);
            }
        });
    }

    /// Start a phase and follow the job it returns.
    pub(super) fn start(&self, id: String, args: Value, toasts: crate::widgets::Toasts) {
        if self.busy.get_untracked() {
            return;
        }
        let me = *self;
        me.busy.set(true);
        me.error.set(None);
        // Starting a run is asking to watch it, so any pinned older run lets
        // go of the surface here.
        me.pinned.set(None);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/op/agent_run", &args).await;
            me.busy.set(false);
            match res {
                Ok(v) => {
                    if v["already_running"].as_bool().unwrap_or(false) {
                        // Amber, not red: the run exists, it is just not a new
                        // one — "one live run per agent" (§2.4).
                        toasts.warn(
                            v["message"]
                                .as_str()
                                .unwrap_or("a run is already in flight")
                                .to_string(),
                        );
                    }
                    if let Some(job_id) = v["job_id"].as_i64() {
                        me.load(job_id);
                    } else {
                        me.error
                            .set(Some(format!("no job id came back for '{id}'")));
                    }
                }
                Err(e) => me.error.set(Some(e.to_string())),
            }
        });
    }
}
