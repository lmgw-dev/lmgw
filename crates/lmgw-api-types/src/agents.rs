//! The agent catalog
//
// Design record: agent-catalog §5.

use serde::{Deserialize, Serialize};

/// `GET /api/agents` — one card per catalog entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentCard {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Free text from the manifest, informational.
    pub version: String,
    /// The manifest's `run.kind`: `chat`, `batch` or `container`. Empty for a
    /// row whose stored manifest could not be read (`error` says why).
    pub kind: String,
    /// The manifest's model alias, template and all — usually
    /// `{{config.model}}`, in which case `effective_model` says what the
    /// stored config resolves it to.
    pub model_alias: String,
    pub effective_model: String,
    /// The tool labels this agent attaches, in manifest order.
    pub labels: Vec<String>,
    pub enabled: bool,
    /// `builtin`, `imported` or `authored`.
    pub source: String,
    /// Every label resolves and every narrowed tool name is currently listed.
    pub requires_ok: bool,
    pub requires: Vec<AgentRequirement>,
    /// Warnings that are not tool gaps.
    // container-runtime §4.3
    pub warnings: Vec<AgentWarning>,
    pub last_run: Option<AgentRunSummary>,
    /// Threads a `chat` agent has opened.
    pub threads: i64,
    /// This agent declares `run.service`, so it serves an app of its own — the card's
    /// `app` chip, and a link to the App tab.
    pub app: bool,
    /// Set when the stored manifest could not be read at all — a row written
    /// by a newer build. The card still lists, saying why, rather than the
    /// whole catalog failing.
    pub error: Option<String>,
}

/// One `tools[]` entry measured against what the gateway offers right now.
///
/// A gap is a warning, never a refusal: an agent is imported before its server
/// is wired, the same way a server is registered before its image is pulled.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentRequirement {
    pub label: String,
    /// The label resolves to a registered server or a built-in toolset.
    pub registered: bool,
    /// Names in `allowed` the source does not list right now.
    pub missing_tools: Vec<String>,
    /// The manifest's install hint, for prefilling the MCP page. lmgw never
    /// fetches anything from it.
    pub install: Option<AgentInstallHint>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentInstallHint {
    /// `git`, `image` or `url`.
    pub kind: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub notes: String,
}

/// `GET /api/agents/{id}` — everything the detail page's three tabs need.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentDetail {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub kind: String,
    pub source: String,
    pub enabled: bool,
    pub model_alias: String,
    pub effective_model: String,
    /// The manifest as stored: the canonical serialization, **as text** — what
    /// the Definition editor loads and what Export writes.
    ///
    /// Text and not a `serde_json::Value` on purpose. `serde_json::Map` is a
    /// `BTreeMap`, so a `Value` here would alphabetize the config schema's
    /// properties and silently re-sort the form the author laid out.
    /// Parse it client-side if a tree is needed, and post it back to
    /// `agent_set` as a **string**: an object argument is accepted but arrives
    /// already re-sorted, and the import report says so.
    pub manifest: String,
    /// Stored config values with every `secret` field replaced by
    /// `{ "has_value": bool }`. The value itself never leaves the process.
    pub config: serde_json::Value,
    /// The config schema flattened into form fields, in document order.
    pub fields: Vec<AgentField>,
    pub requires: Vec<AgentRequirement>,
    pub requires_ok: bool,
    /// The two Responses settings a turn is bounded by, shown next to Apply.
    pub budget: AgentBudget,
    /// The run in flight, if any.
    pub live_job: Option<AgentRunSummary>,
    /// The agent's most recent run, with its outcome (`status`, `error`, tokens,
    /// cost) — the same summary the catalog card carries. While a run is in
    /// flight it is that run. Absent when the agent has never run, when
    /// retention pruned its runs, and on a row this build cannot read.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub last_run: Option<AgentRunSummary>,
    pub threads: i64,
    /// A shipped manifest exists for this id, so "Reset to shipped" applies.
    pub resettable: bool,
    /// The run surface a `batch` or `container` agent offers. `None` for a
    /// `chat` agent.
    pub batch: Option<AgentBatchShape>,
    /// Warnings that are not tool gaps. Start is disabled while any of them carries
    /// `blocks_start`.
    pub warnings: Vec<AgentWarning>,
    /// What a `container` agent runs under. `None` for every other kind.
    pub runtime: Option<AgentRuntime>,
    /// The agent's own token — its name, whether one has been minted, and the scope it
    /// currently carries. **Never the value**:
    /// that comes from `agent_token_get`, which is what Copy token calls.
    pub token: AgentToken,
    /// Service mode, when the manifest declares it.
    /// `None` means no App tab, no proxy route and no `agent:<id>` MCP row.
    pub service: Option<AgentService>,
    /// Where this row was installed from. `None` for a row that came
    /// from a pasted manifest rather than from an image.
    pub provenance: Option<AgentProvenance>,
    /// The dev-server override. While it is set, `/agents/<id>/app/**`
    /// goes there and no container is started for the app; runs and applies
    /// still use the image. Empty means "served from the image".
    pub dev_url: String,
    /// Whether an export of this agent would land on another box, and what the
    /// receiver would have to do if not. The same answer the export file's
    /// `portability` key carries, on the page that offers the download.
    pub portability: AgentPortability,
    /// The gateway's display currency, so a run's cost is printed the way the
    /// Usage page prints every other one.
    pub currency: String,
    pub created_at: String,
    pub updated_at: String,
    /// Set when the stored manifest could not be read at all — a row written by
    /// a newer build. The document still comes back `200` with whatever could
    /// be read, so the Definition editor stays reachable for the very manifest
    /// that needs fixing; a `400` here would lock the client out of it.
    pub error: Option<String>,
}

/// The config format of a field holding a Decision lab question set's id
/// (an `integer` field; empty is no set), which a batch output's `decide`
/// names (labs-store design §4). The form renders it as a select of the
/// owner's question sets.
pub const QUESTION_SET_FORMAT: &str = "lmgw-question-set";

/// One rendered config field. `ty` is `string | integer | number |
/// boolean | array`; `format` is `secret | model_alias | multiline | directory
/// | file` or empty, or [`QUESTION_SET_FORMAT`] on an integer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentField {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub format: String,
    pub title: String,
    pub description: String,
    pub default: Option<serde_json::Value>,
    #[serde(rename = "enum")]
    pub enum_values: Vec<String>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub required: bool,
    /// For a `secret` field: whether one is stored. Always false otherwise —
    /// the value itself is in `config`.
    pub has_value: bool,
    /// For a `directory` or `file` field: `ro` or `rw`, as the **manifest**
    /// declared it. Empty for every other field — the mode is a
    /// property of the agent, never of the form.
    pub access: String,
}

/// The visible bounds a turn runs under: one pair from Settings → Agents & tools,
/// never a second one invented per agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentBudget {
    pub max_tool_calls: u32,
    pub timeout_seconds: u64,
}

/// What a `batch` manifest can actually do, for the Run tab.
///
/// Derived server-side from the manifest **and the stored config**, because
/// both halves matter and only one of them is in the manifest: the review
/// columns are the author's, in the author's order, while the values an
/// editable field may be set to come from the config field `enum_from` names —
/// so widening the taxonomy is a config edit and the table picks it up.
/// A client re-deriving this from the manifest text would also lose the column
/// order, since a JSON object parses into a sorted map here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentBatchShape {
    /// Review-table columns, in the manifest's order.
    pub columns: Vec<String>,
    /// Output fields the reviewer may override, with their allowed values.
    pub editable: Vec<AgentReviewField>,
    /// The manifest declares a per-item model call, so "Dry run" applies.
    pub has_classify: bool,
    /// The manifest declares an apply step, so there is something to write.
    pub has_apply: bool,
    /// The tools Apply reaches, named next to the button so the reviewer knows
    /// what is about to be called.
    pub apply_tools: Vec<String>,
    /// `apply_tools` is the ceiling the agent's token enforces rather than the
    /// calls the step makes — true for a container, whose image decides. The
    /// Run tab says "may call" instead of "calls".
    pub apply_tools_are_ceiling: bool,
    /// The output can classify through a question set (its `decide`); `None`
    /// when it cannot.
    pub decide: Option<AgentDecide>,
}

/// A batch output that classifies through a Decision lab question set
/// (labs-store design §4): the config field holding the set, the set's
/// question it asks, and how the stored config's set reads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentDecide {
    /// The config field holding the set's id ([`QUESTION_SET_FORMAT`]).
    pub field: String,
    /// The set's question that answers the output field: the output field's
    /// own name. It must be a `choice` question, whose options are the
    /// review's values with the fallback last.
    pub question: String,
    /// The config field the enum comes from without a set (`enum_from`),
    /// which a run with a set does not read.
    pub enum_from: Option<String>,
    /// The set the stored config names; `None` classifies with the model
    /// call, as without `decide`.
    pub set: Option<i64>,
    /// That set's name.
    pub set_name: Option<String>,
    /// The threshold the set keeps for its question (0 when it has none),
    /// measured or not: see `measured`.
    pub threshold: Option<f64>,
    /// The threshold was measured on a labelled set run. Only a measured
    /// threshold is applied: an answer whose confidence is below it needs
    /// attention. An unmeasured one is shown, not applied (Decision 10).
    pub measured: bool,
    /// The config field the agent's model alias is (`{{config.<field>}}`),
    /// when it is one: a classify needs it or a set, so the Run tab blocks
    /// Classify while both are empty.
    pub model_field: Option<String>,
    /// Why the stored config's set cannot classify (gone, no such question,
    /// not a choice, no default model); a run refuses with it.
    pub problem: Option<String>,
}

/// The question set that classified a batch row (labs-store design §4),
/// stamped on the row as the run read the set: its id, its name and its
/// `updated_at`. "Save as labelled set" saves a run's rows into the set
/// that classified them; a row the model call classified has none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentRowSet {
    /// The set's preset id.
    pub id: i64,
    /// Its name then.
    pub name: String,
    /// Its `updated_at` then: the version of the set that answered.
    pub updated_at: String,
    /// A digest of the output question's definition (type, text, options
    /// with descriptions, levels) as it was asked. "Save as labelled set"
    /// reports the set changed only when the question's digest differs, not
    /// on any `updated_at` bump (a threshold write). Empty on a row stamped
    /// without one, which reads as unchanged.
    pub digest: String,
}

/// One warning that is not a tool gap.
///
/// Beside `requires`, of the same shape and rendered in the same place. The
/// Start gate is `requires_ok` **and** no warning with `blocks_start`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentWarning {
    /// A stable identifier — `container_without_image`, `podman_unavailable`,
    /// `image_absent_pull_never`, `local_image_on_import`,
    /// `secrets_dir_fallback`, `install_image_mismatch`, `dev_url_active`.
    pub code: String,
    pub message: String,
    pub blocks_start: bool,
}

/// What a `container` agent runs under.
///
/// Every limit is here with its default printed next to it on the Run tab:
/// a container's bounds are shown, never guessed, like a run's tool-call and
/// wall-clock budget. `0` means "no
/// limit" everywhere except `stop_grace_seconds`, where it means SIGKILL at
/// once — the page says which.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentRuntime {
    pub image: String,
    /// `never` | `missing` | `always`.
    pub pull: String,
    /// Which phases the image implements, in declaration order.
    pub phases: Vec<String>,
    pub memory_mb: u64,
    pub cpus: f64,
    pub pids: u64,
    pub deadline_seconds: u64,
    pub stop_grace_seconds: u64,
    pub read_only: bool,
    /// The phases whose `output` event is validated against a declared schema.
    /// A phase absent here is unvalidated, which the Run tab says out loud
    /// rather than leaving the reader to assume a check that is not happening.
    pub output_validated: Vec<String>,
    /// `podman --version` answered on this box.
    pub podman: bool,
    /// Why it did not, when it did not.
    pub podman_note: String,
}

/// Service mode as the App tab describes it.
///
/// Everything a bound here is read from is a manifest field with its value
/// printed: the idle window, the start timeout and the health path are the
/// manifest's, and `0` means what it says everywhere else — never idle-stop,
/// and wait as long as the start takes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentService {
    /// The in-container port the manifest declares.
    pub port: u32,
    /// `run.service.health_path`. Empty = a TCP connect instead of an HTTP GET.
    pub health_path: String,
    /// `0` = never idle-stop, exactly as an MCP server's `idle_seconds` means it.
    pub idle_seconds: i64,
    /// `0` = wait as long as the container takes.
    pub start_timeout_seconds: u64,
    /// `run.provides.mcp`, when declared: the path inside the container that
    /// speaks MCP, registered as the `agent:<id>` server.
    pub provides_mcp: Option<String>,
    /// The origin this agent's UI is served on, as a URL with its slash:
    /// `http://board.localhost:8001/`. The container's
    /// own `LMGW_APP_ORIGIN` is the same origin *without* the slash — an
    /// origin to concatenate onto, rather than an `href`.
    pub origin: String,
    /// `<id>.<suffix>` resolves on the machine lmgw is running on — the server
    /// asked the resolver when it rendered this. False is not an error: the
    /// App tab prints the `/etc/hosts` line that fixes it, and Chrome and
    /// Firefox resolve `*.localhost` regardless of what the system says.
    pub origin_resolves: bool,
    /// A start is in flight right now.
    pub starting: bool,
    /// The container is up and answering.
    pub running: bool,
    /// The host port this start published on — ephemeral, per start. `0` when
    /// nothing is running.
    pub host_port: u16,
    pub container: String,
    /// When the running container was started.
    pub started_at: Option<String>,
    /// How long ago the last proxied request finished. `None` when nothing is
    /// running.
    pub idle_seconds_now: Option<u64>,
    /// Requests in flight, which is what the idle sweep skips on.
    pub in_flight: usize,
    /// The container's own log tail, read while it is running. Empty when
    /// nothing is up — `podman logs` on a container that is gone has nothing to
    /// say.
    pub log_tail: String,
    /// How many lines `log_tail` is at most, printed beside
    /// it so nobody has to guess whether they are seeing all of it.
    pub log_tail_lines: usize,
    /// The host mounts this agent's container holds, in the manifest's field order and
    /// only the ones that are bound.
    ///
    /// The App tab lists them. `host` is filled for an `Admin` reader and left
    /// out for the container reading its own row, which is told
    /// `/lmgw/mounts/<field>` and nothing else.
    pub mounts: Vec<AgentServiceMount>,
}

/// One bound mount as a reader is shown it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentServiceMount {
    /// The config field that names the slot.
    pub field: String,
    /// The canonical host path — **`Admin` only**. `None` is what the agent's
    /// own view sees, and it is absent from the JSON rather than empty: a
    /// container is not told there is a path it is not being given.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub host: Option<String>,
    /// `/lmgw/mounts/<field>` — what the container sees, for every reader.
    pub inside: String,
    /// `directory` | `file`, from the field's `format`.
    pub kind: String,
    /// `ro` | `rw`, as the manifest declared it.
    pub access: String,
}

/// Where this row's document came from.
///
/// `None` on the detail document for a row nobody installed from an image — an
/// agent written in the Definition editor has no package and says so by having
/// no provenance, rather than by carrying one full of empty strings.
///
/// `installed_at` and `pulled_at` are two facts on purpose: the first is when
/// the row was written from the image, the second when the digest was last
/// read. `agent_pull` moves only the second.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentProvenance {
    /// The image reference as it was given at install, tag and all.
    pub image: String,
    /// `podman image inspect --format '{{.Digest}}'` at the last read. Absent
    /// when podman could not say — never a placeholder that reads like a digest.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub digest: Option<String>,
    /// Where in the image the manifest was found (`/lmgw/agent.json`).
    pub manifest_path: String,
    pub installed_at: String,
    pub pulled_at: String,
}

/// Whether the export of this agent lands on another box.
///
/// Shown before the download, never enforced: exporting to the same box is the
/// common case, and the wrong answer is a file that looks complete while naming
/// an image only the exporter has.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentPortability {
    /// Nothing about this agent is local to this machine.
    pub portable: bool,
    /// What the receiver would have to do, one sentence each. Empty when
    /// `portable`.
    pub notes: Vec<String>,
}

/// The agent's credential as the detail page describes it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentToken {
    /// `agent:<id>` — the name Logs and the usage page print.
    pub name: String,
    /// A token has been minted. `false` until the first run or the first Copy
    /// token: an agent that is never run never mints a credential.
    pub has_value: bool,
    /// The derived scope in words — "token: any model", or the aliases it is
    /// fenced to. Shown next to Copy token so the scope is never invisible.
    pub scope_note: String,
}

/// One editable output field. An empty `options` means free text: there is no
/// enum behind it to offer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentReviewField {
    pub field: String,
    pub options: Vec<String>,
}

/// One `agent_run` job as the Runs tab lists it.
///
/// The universal job fields plus the run's own `detail`/`result`, whose inner
/// shape belongs to the executor rather than to this DTO — the same
/// split `JobView` already makes for every other job kind.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentRunSummary {
    pub job_id: i64,
    pub agent_id: String,
    /// `list`, `classify`, `rerun` or `apply`.
    pub phase: String,
    /// `queued | running | done | failed | canceled`.
    pub status: String,
    pub done: u64,
    pub total: Option<u64>,
    pub percent: Option<u64>,
    pub stage: String,
    pub detail: serde_json::Value,
    /// Prompt + completion tokens the run spent, from its result.
    /// `None` until it has one — "not known yet", which is not zero.
    pub tokens: Option<u64>,
    /// What those tokens cost, in micro-units of the gateway's currency.
    /// `None` is "nobody could price this", which is also not zero.
    pub cost_micro: Option<i64>,
    /// The model calls the run made, from its result: calls answered without
    /// an error. `None` until it has a result. `Some(0)` with no `cost_micro`
    /// and no tokens is a run that had nothing to price; `Some(0)` can also
    /// come with a cost, from calls that failed or were stopped after their
    /// rows were priced.
    pub model_calls: Option<u64>,
    /// Wall clock from claim to finish. `None` while it is still going.
    pub duration_ms: Option<i64>,
    pub error: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// `GET /api/agents/runs/{job_id}` — the job plus its rows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentRunDetail {
    pub job: AgentRunSummary,
    /// The reviewed rows: the executor's live buffer while running, the job
    /// result after. Rows are deliberately **not** pushed through the generic
    /// jobs feed.
    pub rows: Vec<serde_json::Value>,
    /// The apply step's structured output and the run's usage total, once the
    /// run has finished.
    pub result: Option<serde_json::Value>,
    /// The agent's current review shape, so the table can be drawn from a run
    /// alone — including one finished last week, which is what "reopen this
    /// run's review" needs. For a ledger run its `columns` carry the undeclared
    /// keys the events brought, appended after the declared ones in first-seen
    /// order.
    pub batch: Option<AgentBatchShape>,
    /// The run log: `log` events, rejected events, and the lines lmgw itself
    /// wrote. Empty for an in-process run, which has none yet.
    pub log: Vec<String>,
}

/// `POST /api/agents/import` and `lmgw__agent_set` — the same report either
/// way. Errors are not in here: they are the 400 body (`ApiError`), because a
/// manifest that does not validate was not imported at all.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentImportReport {
    pub ok: bool,
    pub id: String,
    /// A missing server label or tool: the agent is saved, Start is disabled.
    pub warnings: Vec<String>,
    pub requires: Vec<AgentRequirement>,
    /// An existing agent was overwritten (`replace=1`), keeping its config.
    pub replaced: bool,
    /// Stored config keys the **new** manifest no longer declares, removed with
    /// the manifest that declared them (the same rule applies to every replace). Keeping them
    /// would leave a row that fails validation
    /// on every run and cannot be fixed from its own config form.
    pub dropped_config: Vec<String>,
    /// Mount slots this agent has and nothing has bound: an
    /// export never carries a host path, so an imported agent arrives with its
    /// `directory` and `file` fields empty and the importer says which folder.
    pub config_unbound: Vec<String>,
    /// `validate_only=1`: the same report, nothing written.
    pub validate_only: bool,
}

/// `POST /api/agents/{id}/runs` — a run opened from outside lmgw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentRunOpened {
    /// The run's job id, for `/api/agents/runs/{job_id}` and its events and
    /// close routes.
    pub run: i64,
    /// How long the run has before lmgw ends it; `0` is no deadline.
    pub deadline_seconds: u64,
}

/// `POST /api/op/agent_install` and `lmgw__agent_install` — the import report
/// of the manifest read out of the image (what `agent_set` returns), and where
/// it came from. `warnings` can carry a line a pasted manifest never earns: a
/// digest podman could not report, or a `run.image` other than the image
/// installed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentInstalled {
    #[serde(flatten)]
    pub report: AgentImportReport,
    /// The image reference that was installed, trimmed.
    pub image: String,
    /// The image's digest; absent when podman could not report one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub digest: Option<String>,
    /// This call downloaded the image: `false` when it was already on the box
    /// (or `always` found it unchanged), so a download is never left to be
    /// inferred from how long the call took.
    pub pulled: bool,
    /// Where the manifest was read from inside the image: `/lmgw/agent.json`.
    pub manifest_path: String,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agent_delete` and `lmgw__agent_delete`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentDeleted {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// Chat threads that were opened through the agent and were kept, on the
    /// Chat page, without it.
    pub threads_kept: u64,
    /// What was done, for a person to read.
    pub message: String,
    /// How to bring it back: `agents_restore` for a shipped agent, importing
    /// the exported file for any other.
    pub hint: String,
}

/// `POST /api/op/agent_run` and `lmgw__agent_run` — one phase of a run was
/// started, or a run of the agent is already in flight and this is that one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentRunStarted {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// The run's job id, for `GET /api/agents/runs/{job_id}`: the new run's,
    /// or the one in flight when `already_running`.
    pub job_id: i64,
    /// The phase of the run `job_id` names — the one asked for, or when
    /// `already_running` the in-flight run's own: `list`, `classify`, `rerun`,
    /// `apply`, or `run` for a `container` agent.
    pub phase: String,
    /// "One live run per agent" is a report, not a refusal: a run of this
    /// agent was in flight, `job_id` is that one and nothing was started.
    pub already_running: bool,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/agents/{id}/runs` refused with `409`: the agent already has a
/// run in flight, and `run` is its job id, so the caller can watch that one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentRunConflict {
    /// Always `already_running`.
    pub code: String,
    pub message: String,
    /// The job id of the run that is in flight.
    pub run: i64,
}

/// `POST /api/op/agent_pull` — the agent's image was fetched again, and this is
/// whether it moved. Nothing is adopted: when the digest moved and the new image
/// carries a different manifest, `agent_reimport` is the owner saying yes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentPulled {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// The image reference that was pulled.
    pub image: String,
    /// The digest podman held before the pull; absent when it held none, or
    /// could not report one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub old_digest: Option<String>,
    /// The digest after the pull; absent when podman could not report one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub new_digest: Option<String>,
    /// The digest the agent's row recorded when it was installed or last
    /// pulled — different from `old_digest` whenever something else on the box
    /// moved the image. Absent for a row that never recorded one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub recorded_digest: Option<String>,
    /// Whether the image changed. Both digests known: they differ. No digest
    /// before and one now: `true`, the image arrived. Absent when podman could
    /// not report a digest after the pull (unknown).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub changed: Option<bool>,
    /// Only asked when `changed` is `true`: the new image's manifest differs from
    /// the stored one (`true`), is the same (`false`), or could not be compared
    /// (`null`, and `note` says why). `null` too when nothing moved.
    pub manifest_differs: Option<bool>,
    /// What the comparison found, for a person to read; empty when nothing
    /// moved.
    pub note: String,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agent_reimport` — the install path ran again over an existing
/// row: the manifest was replaced and the config kept. The import report of the
/// manifest read out of the image, and whether it differed from the stored one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentReimported {
    #[serde(flatten)]
    pub report: AgentImportReport,
    /// The image the manifest was read from.
    pub image: String,
    /// The image's manifest differs from the one that was stored. `false`
    /// also adds a line to `warnings` saying nothing about the agent changed.
    pub changed: bool,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agent_dev_url_set` — the agent's app is served from a dev
/// server (`dev_url` is its URL), or from its image again (`dev_url` is empty).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentDevUrlSet {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// The dev server's URL as stored; empty when the override was cleared.
    pub dev_url: String,
    /// Setting a URL stops a running app container, since it was started from
    /// the image the URL replaces: the name of the container that was stopped.
    /// Absent when none was running, and when the override was cleared.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub stopped: Option<String>,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agent_config_set` — the stored config after a sparse patch.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentConfigSaved {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// The agent's token scope, re-derived from the saved config, in words:
    /// `token: any model` or the aliases it is fenced to.
    pub token_scope: String,
    /// The folder and file slots whose stored path changed, cleared ones
    /// included.
    pub mounts_repointed: Vec<String>,
    /// A running app container holds the old folder, so a re-pointed slot stops
    /// it: the name of the container that was stopped; absent when none was
    /// running.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub service_stopped: Option<String>,
    /// What is now stored, with every `secret` field replaced by
    /// `{ "has_value": bool }` — the merged values, not the patch.
    pub config: serde_json::Value,
    /// Names in `clear` that the manifest does not declare. Clearing one is
    /// legitimate (it is how a value orphaned by a manifest change goes away),
    /// but a typo in `clear` looks exactly like it.
    pub cleared_undeclared: Vec<String>,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agent_enable` — the agent was enabled or disabled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentEnabled {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// The state it is in now.
    pub enabled: bool,
    /// Disabling stops a running app container: the name of the container that
    /// was stopped; absent when none was running or the agent was enabled. The
    /// sentence is in `message`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub service_stopped: Option<String>,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agent_duplicate` — a copy of an agent under a new id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentDuplicated {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The copy's id.
    pub id: String,
    /// The copy's name: the one asked for, or the original's with ` (copy)`.
    pub name: String,
    /// The `secret` config fields left out of the copy: a copy must not
    /// silently inherit a credential.
    pub config_omitted: Vec<String>,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agent_reset` — a shipped agent is back on its built-in
/// manifest, keeping its config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentReset {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// Stored config keys the shipped schema no longer declares, dropped so the
    /// next Start does not fail on them.
    pub dropped_config: Vec<String>,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agent_run_cancel` — the agent's run was asked to stop. The
/// executor stops between items and keeps the rows it has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentRunCancelled {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agent_token_get` — the agent's own token, in the clear: a read
/// of the column that already holds it (minted first if the agent had none).
/// `agent_token_rotate` answers with [`AgentTokenIssued`].
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentTokenRead {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// `agent:<id>` — the name Logs and the usage page print.
    pub name: String,
    /// The bearer token. Secret: it is shown here and nowhere else.
    pub token: String,
    /// `all` or `allow`: whether the token is fenced to model aliases.
    pub scope_mode: String,
    /// The model aliases the token is fenced to, one per line; empty for `all`.
    pub scope_patterns: String,
    /// The scope in words, as the detail page prints it.
    pub scope_note: String,
    /// What was done, for a person to read.
    pub message: String,
}

impl std::fmt::Debug for AgentTokenRead {
    /// Everything but the token, which a log line must not carry.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentTokenRead")
            .field("ok", &self.ok)
            .field("id", &self.id)
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .field("scope_mode", &self.scope_mode)
            .field("scope_patterns", &self.scope_patterns)
            .field("scope_note", &self.scope_note)
            .field("message", &self.message)
            .finish()
    }
}

/// `POST /api/op/agent_token_rotate` — the agent's new token, in the clear, and
/// what the rotation did to the app container that was holding the old one.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentTokenIssued {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// `agent:<id>` — the name Logs and the usage page print.
    pub name: String,
    /// The bearer token. Secret: it is shown here and nowhere else.
    pub token: String,
    /// `all` or `allow`: whether the token is fenced to model aliases.
    pub scope_mode: String,
    /// The model aliases the token is fenced to, one per line; empty for `all`.
    pub scope_patterns: String,
    /// The scope in words, as the detail page prints it.
    pub scope_note: String,
    /// A rotation stops the app container that was holding the old token: the
    /// name of the container that was stopped; absent when none was running.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub service_stopped: Option<String>,
    /// A rotation that could not re-derive the scope still rotated: the token
    /// above is the live one, and this says what did not happen. Absent
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub warning: Option<String>,
    /// What was done, for a person to read.
    pub message: String,
}

impl std::fmt::Debug for AgentTokenIssued {
    /// Everything but the token, which a log line must not carry.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentTokenIssued")
            .field("ok", &self.ok)
            .field("id", &self.id)
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .field("scope_mode", &self.scope_mode)
            .field("scope_patterns", &self.scope_patterns)
            .field("scope_note", &self.scope_note)
            .field("service_stopped", &self.service_stopped)
            .field("warning", &self.warning)
            .field("message", &self.message)
            .finish()
    }
}

/// `POST /api/op/agent_service_start` — the agent's app container is up, and
/// healthy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentServiceStarted {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// Always `true`.
    pub running: bool,
    /// The container's name.
    pub container: String,
    /// The loopback port the container's app is published on.
    pub host_port: u16,
    /// Where the app answers now: the agent's own origin, which is what a
    /// reader opens (the host port is lmgw's business).
    pub origin: String,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agent_service_stop` — the agent's app container is not
/// running. Stopping something that is not running is success.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentServiceStopped {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// Always `false`.
    pub running: bool,
    /// The name of the container that was stopped or collected; absent when
    /// there was none.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub stopped: Option<String>,
    /// A start was in flight and was interrupted, rather than a running
    /// container being stopped.
    pub cancelled_start: bool,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agent_service_log` — the tail of the agent's app container's
/// log, as long as the reader asked for. Nothing running is an answer, not an
/// error: the container may have idle-stopped between the page load and the
/// click.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentServiceLog {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    pub id: String,
    /// How many lines `log` holds. Fewer than asked for when the container has
    /// logged less (`0` asked for is the whole log).
    pub lines: u64,
    /// Whether the app container is running; `log` is empty when it is not.
    pub running: bool,
    /// The container the log is read from. Absent when nothing is running.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub container: Option<String>,
    /// The log's tail.
    pub log: String,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agent_open_chat` — a new Chat thread was opened from a `chat`
/// agent's manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentChatOpened {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The agent's id.
    pub id: String,
    /// The new thread's id.
    pub thread_id: i64,
    /// The model alias the thread was opened on: the manifest's, rendered
    /// against the config.
    pub model: String,
    /// The tool labels the thread attaches, in manifest order.
    pub tools: Vec<String>,
    /// Where the dashboard navigates to: `/chat?t=<thread_id>`.
    pub url: String,
    /// What is off about a thread that did open: an alias the gateway does not
    /// serve, a tool server that is not registered, manifest sampling knobs a
    /// thread has no column for.
    pub warnings: Vec<String>,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/agents_restore` — the shipped agents the catalog was missing
/// are back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentsRestored {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The ids that were put back; empty when every shipped agent was already
    /// there.
    pub restored: Vec<String>,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/agents/runs/{job_id}/events` — what became of the batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentEventsApplied {
    /// Always `true`: rejected events are reported below, not failed.
    pub ok: bool,
    /// How many events went into the run.
    pub applied: usize,
    /// One line per event the run did not take (unparseable, or refused by
    /// the run's rules), in the order they were sent.
    pub rejected: Vec<String>,
}

/// `GET /api/agents/{id}/export` — the `<id>.agent.json` file: the agent's
/// manifest with an envelope around it. The manifest's fields sit at the top
/// level of the file, in the order the manifest has them; the envelope's own
/// fields follow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "schema",
    derive(schemars::JsonSchema),
    schemars(rename = "AgentExport")
)]
pub struct AgentExport<M = serde_json::Map<String, serde_json::Value>> {
    /// The manifest's own fields (`id`, `name`, `kind`, `tools`, `config`,
    /// …), at the top level of the file.
    #[serde(flatten)]
    pub manifest: M,
    /// RFC 3339 time of the export.
    pub exported_at: String,
    /// The lmgw version that wrote the file.
    pub lmgw_version: String,
    /// The `secret` config fields left out, so the receiver knows what to
    /// fill in. Absent when there are none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub config_omitted: Option<Vec<String>>,
    /// The folder and file slots left out: they are paths on the exporting
    /// machine, so the receiver chooses its own. Absent when there are none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub config_unbound: Option<Vec<String>>,
    /// The non-secret config values; only with `include_config=1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub config_values: Option<serde_json::Map<String, serde_json::Value>>,
    /// Whether the file runs on another machine as it is, and what the
    /// receiver would have to do if not.
    pub portability: AgentPortability,
}
