//! The agent catalog (agent-catalog design).
//!
//! An **agent** is a manifest: one JSON document that names a model, prompts,
//! the MCP tools it may reach, a config form and a run shape. Agents live in a
//! catalog that is stored, imported and exported as data — adding one never
//! rebuilds lmgw.
//!
//! What lives where:
//!
//! - [`manifest`] — the schema-v1 types, their validation, and the §2.6 config
//!   subset the Run tab's form is drawn from.
//! - [`batch`] — the `batch` run kind: the [`JobExecutor`](crate::jobs::JobExecutor)
//!   behind the `agent_run` jobs, the review rows, and the apply step (§2.4).
//! - [`container`] — the `container` run kind: `podman run` per phase, the
//!   JSONL transport, cancel/deadline and boot reconciliation
//!   (container-runtime §6).
//! - [`mounts`] — where a bound host path may point, and the refusal each
//!   rule produces (mounts §5.3).
//! - [`template`] — `{{path}}` substitution, and nothing else (§2.3).
//! - [`service`] — the `run.service` half of the container kind
//!   (container-runtime §3.3): the on-demand start, the idle stop, and the
//!   `agent:<id>` MCP row a `provides.mcp` manifest earns.
//! - [`seed`] — the built-ins that ship embedded, and the once-only seed.
//! - [`chat`] — the `chat` run kind: one agent into one Chat thread (§2.5).
//! - [`crate::web::api_agents`] — the HTTP face: reads, ops, import/export.
//!
//! [`JOB_KIND`] and [`job_key`] name the jobs rows a run creates, and every
//! manifest either reads has been through [`manifest::load`].

pub mod batch;
pub mod chat;
pub mod container;
pub mod ledger;
pub mod manifest;
pub mod mounts;
pub mod package;
pub mod seed;
pub mod service;
pub mod template;
pub mod token;

use serde_json::Value;

use crate::mcp::exec;
use crate::state::SharedState;
use crate::store::AgentRow;

pub use manifest::Manifest;
pub use token::AgentIdentity;

/// The `jobs.kind` every agent run is recorded under (§3).
///
/// The string [`JobKind::AgentRun`](crate::jobs::JobKind::AgentRun) renders to,
/// and the one the reads in `api_agents` query by. Kept here rather than
/// spelled out at each call site: `jobs.kind` is free text in the schema (see
/// `0019_jobs.sql`), so a typo would be a query that silently matches nothing.
pub const JOB_KIND: &str = "agent_run";

/// The `jobs.key` for one agent: finished rows keep it, so "runs of this agent"
/// is `WHERE kind = 'agent_run' AND key = ?`, and the partial unique index on
/// `(kind, key)` gives "one live run per agent" for free (§2.4).
pub fn job_key(agent_id: &str) -> String {
    format!("agent:{agent_id}")
}

/// The KV key tracking which built-in ids have been seeded (§3).
pub const SEEDED_KEY: &str = "agents:seeded";

/// The KV key holding `{ "<agent id>": { "url": …, "why": … } }` for every
/// `dev_url` lmgw had to clear (container-runtime §3.4, origins §4.7).
///
/// Persistent, not in-memory: a `bind_addr` change only takes effect on the
/// next start, so the owner reads the consequence after a restart — an
/// in-process flag would be gone exactly when it was needed. A note written
/// before the reason travelled with the url is a bare string and is still
/// read; see [`service::cleared_dev_urls`].
pub const DEV_URL_CLEARED_KEY: &str = "agents:dev_url_cleared";

/// A catalog row with its manifest already parsed.
#[derive(Debug, Clone)]
pub struct Agent {
    pub row: AgentRow,
    pub manifest: Manifest,
}

impl Agent {
    /// Parse a stored row.
    ///
    /// A row written by a newer build must not crash an older one: the failure
    /// is reported against the row's id, and every caller turns it into a
    /// visible error rather than a panic (§4.1).
    pub fn from_row(row: AgentRow) -> Result<Self, String> {
        let manifest = manifest::load(&row.manifest)
            .map_err(|e| format!("agent '{}' has an unreadable manifest: {e}", row.id))?;
        Ok(Self { row, manifest })
    }

    /// Stored config values as an object, whatever the column holds.
    pub fn config_values(&self) -> serde_json::Map<String, Value> {
        serde_json::from_str::<Value>(&self.row.config)
            .ok()
            .and_then(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// The `config` root a template resolves against: stored values over schema
    /// defaults (§2.3), with every mount field reading as its container path.
    ///
    /// `{{config.notes}}` renders `/lmgw/mounts/notes` and never the folder on
    /// this box (mounts §5.6): a template's product is a prompt, a tool
    /// argument or a model alias, and all three are read by something on the
    /// far side of the container wall. It is not *refused* in a prompt the way
    /// a secret is — a container path tells a model nothing about the host.
    ///
    /// The owner's own reads do not come through here: the detail document is
    /// built from [`manifest::masked_values`] over the stored values, and the
    /// host path is taken out of it for the container's copy alone
    /// (principals §3.10).
    pub fn effective_config(&self) -> Value {
        let fields = self.manifest.fields().unwrap_or_default();
        let values = manifest::effective_values(&fields, &self.config_values());
        mounts::as_container_paths(&self.manifest, values)
    }

    /// Apply a sparse patch over the stored config **for this run or thread
    /// only** — nothing is written back to the catalog.
    ///
    /// The Run tab's form is what a start uses, and Save config is what sets
    /// the defaults a start falls back to. Picking a different model for one
    /// conversation is an ordinary thing to want, and it must not rewrite the
    /// agent to get it. Merged rather than replaced, with
    /// [`manifest::merge_values`]' secret rule intact: a patch that omits a
    /// secret keeps the stored one, exactly as a save does.
    ///
    /// Validated as a **complete** config, because that is what it is about to
    /// be run as — the error names the field, which is the form's business.
    pub fn override_config(
        &mut self,
        patch: &serde_json::Map<String, Value>,
    ) -> Result<(), String> {
        if patch.is_empty() {
            return Ok(());
        }
        let fields = self.manifest.fields().map_err(|e| e.join("; "))?;
        let merged = manifest::merge_values(&fields, &self.config_values(), patch);
        manifest::validate_values(&fields, &merged)?;
        self.row.config = Value::Object(merged).to_string();
        Ok(())
    }

    pub fn job_key(&self) -> String {
        job_key(&self.row.id)
    }
}

// ---------------------------------------------------------------------------
// Tool requirements (§5)
// ---------------------------------------------------------------------------

/// What one `tools[]` entry needs and whether the gateway has it.
///
/// A gap is a **warning, never a refusal**: an MCP server is registered before
/// its image is pulled, and an agent is imported before its server is wired,
/// for the same reason. The card shows the gap and the Run tab disables Start
/// with the reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Requirement {
    pub label: String,
    /// The label resolves to a registered server or a built-in toolset.
    pub registered: bool,
    /// Names in `allowed` that the source does not list right now.
    pub missing_tools: Vec<String>,
}

impl Requirement {
    pub fn ok(&self) -> bool {
        self.registered && self.missing_tools.is_empty()
    }
}

/// Every label the gateway can offer right now, with the exposed tool names
/// under it. Computed once so a catalog listing does not re-derive it per
/// agent.
///
/// Listing the southbound half goes through
/// [`McpManager::list_tools`](crate::mcp::McpManager::list_tools), the same
/// call the northbound `tools/list` and the Chat thread picker make, so an
/// `autostart = false` server's tools count as present here exactly as they do
/// there.
pub struct ToolSurface {
    labels: Vec<(String, Vec<String>)>,
}

impl ToolSurface {
    pub async fn load(state: &SharedState) -> Self {
        let snap = state.snapshot();
        let mut labels: Vec<(String, Vec<String>)> = Vec::new();

        // The built-in labels. `lmgw` is listed with its whole catalog,
        // not with what the self-admin mode gate currently exposes: attaching
        // the label cannot widen the Setting (§2.5), and an agent should not
        // look broken because self-admin happens to be read-only today.
        labels.push((
            exec::SELF_ADMIN_LABEL.to_string(),
            crate::mcp::selfadmin::full_catalog()
                .iter()
                .filter_map(|(entry, _)| {
                    entry
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect(),
        ));
        labels.push((
            exec::DOCS_LABEL.to_string(),
            crate::mcp::docs::list()
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect(),
        ));
        labels.push((
            exec::KB_LABEL.to_string(),
            crate::mcp::kb::list()
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect(),
        ));

        let agg = state.mcp.list_tools(&snap).await;
        for server in snap.mcp_servers.values() {
            let names: Vec<String> = agg
                .reverse
                .iter()
                .filter(|(_, (sid, _))| *sid == server.id)
                .map(|(exposed, _)| exposed.clone())
                .collect();
            let label = exec::server_label(server);
            // A server addressed by its name even though it has a prefix is a
            // near miss `mcp::exec::resolve` already honors; honor it here too,
            // so the card and the run agree.
            if label != server.name {
                labels.push((server.name.clone(), names.clone()));
            }
            labels.push((label, names));
        }
        Self { labels }
    }

    fn names(&self, label: &str) -> Option<&[String]> {
        self.labels
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, n)| n.as_slice())
    }

    /// Check one manifest's `tools[]` against this surface.
    pub fn check(&self, tools: &[manifest::ToolRef]) -> Vec<Requirement> {
        tools
            .iter()
            .map(|t| match self.names(&t.label) {
                None => Requirement {
                    label: t.label.clone(),
                    registered: false,
                    missing_tools: t.allowed.clone().unwrap_or_default(),
                },
                Some(have) => Requirement {
                    label: t.label.clone(),
                    registered: true,
                    missing_tools: t
                        .allowed
                        .iter()
                        .flatten()
                        .filter(|want| !have.iter().any(|h| h == *want))
                        .cloned()
                        .collect(),
                },
            })
            .collect()
    }

    /// Every exposed tool name one manifest's `tools[]` resolves to right now
    /// — the union of each entry's `allowed`, or the whole label's current
    /// surface when `allowed` is absent (container-runtime §3.1).
    ///
    /// This is the list the agent's token is filtered against on `/mcp`. It is
    /// recomputed per call rather than stored, for the same reason
    /// `disabled()` is evaluated per call: a list is a snapshot and a call is
    /// not, and a label's surface changes when its server does.
    pub fn allowed(&self, tools: &[manifest::ToolRef]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for t in tools {
            let names: Vec<String> = match &t.allowed {
                Some(allowed) => allowed.clone(),
                None => self.names(&t.label).unwrap_or_default().to_vec(),
            };
            for n in names {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        out
    }

    /// The labels an author can choose from, for an error that names the
    /// alternatives.
    pub fn labels(&self) -> Vec<String> {
        let mut out: Vec<String> = self.labels.iter().map(|(l, _)| l.clone()).collect();
        out.dedup();
        out
    }
}

/// The warning lines a set of requirements produces, in the import report's
/// vocabulary (§5).
pub fn requirement_warnings(reqs: &[Requirement], surface: &ToolSurface) -> Vec<String> {
    let mut out = Vec::new();
    for r in reqs {
        if !r.registered {
            out.push(format!(
                "no MCP server with label '{}' is registered on this gateway (available: {}). \
                 The agent is saved; Start stays disabled until it is.",
                r.label,
                surface.labels().join(", ")
            ));
        } else if !r.missing_tools.is_empty() {
            out.push(format!(
                "the '{}' toolset does not currently list: {}",
                r.label,
                r.missing_tools.join(", ")
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Warnings (container-runtime §4.3)
// ---------------------------------------------------------------------------

/// A warning that is not a tool gap.
///
/// [`Requirement`] is a *tool-gap* list with no room for anything else, so the
/// second list lives beside it and renders in the same place on the card and
/// the Run tab. `blocks_start` is the part that matters: the new Start gate is
/// `requires_ok && warnings.none(blocks_start)`.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentWarning {
    /// A stable identifier, so the UI and a test can name one.
    pub code: &'static str,
    pub message: String,
    pub blocks_start: bool,
}

/// The catalog §5.3 sentence `apply.turn` now carries (container-runtime §4.3).
///
/// A **warning, not a parse error**: `Manifest::errors` feeds `validate`, which
/// feeds `load`, which feeds `Agent::from_row` — so erroring here would brick
/// every stored manifest that has one, including the shipped mail labeler an
/// owner is upgrading from.
pub const APPLY_TURN_MESSAGE: &str =
    "apply may not run a model turn: the apply step writes, and a model deciding what to write \
     is neither deterministic nor reviewable. Use a direct tool call, a `script`, or \
     `run.kind: container`.";

impl AgentWarning {
    fn new(code: &'static str, message: impl Into<String>, blocks_start: bool) -> Self {
        Self {
            code,
            message: message.into(),
            blocks_start,
        }
    }
}

/// The **manifest half** (§4.3): a pure function of the document plus the row's
/// `source`, so it is unit-testable without a gateway and can run inside the
/// import report.
///
/// WP2 computes the two container codes; WP3 adds `apply_turn` and
/// `script_without_output`, and `builtin_update_available` comes from the §5.1
/// upgrade pass in the runtime half. The split is what lets each land without
/// moving the others.
pub fn manifest_warnings(m: &Manifest, source: &str) -> Vec<AgentWarning> {
    let mut out = Vec::new();
    // The two step-shaped warnings apply to the batch pipeline, which is the
    // only place a `turn` or a `script` can sit in an apply step.
    if let Some(step) = m.apply_step() {
        if step.is_turn() {
            out.push(AgentWarning::new("apply_turn", APPLY_TURN_MESSAGE, true));
        }
        if step.is_script() && step.output.is_none() {
            out.push(AgentWarning::new(
                "script_without_output",
                "the apply script declares no 'output' schema, so whatever it returns is stored \
                 unchecked. Declare run.apply.output and a script that answers with the wrong \
                 shape fails the run instead.",
                false,
            ));
        }
    }
    // A mount field is a slot in a container's filesystem (mounts §5.1), and a
    // `chat` or `batch` manifest has no container to put one in: the form would
    // ask the owner for a folder that nothing ever binds. Blocking, because
    // running it would silently do less than the manifest says.
    if m.kind() != "container" {
        for f in m.mount_fields() {
            out.push(AgentWarning::new(
                "mount_field_without_container",
                format!(
                    "'{}' is a {} field, but this agent has no container to mount it into",
                    f.name,
                    f.kind.as_str()
                ),
                true,
            ));
        }
        return out;
    }
    match m.image() {
        None => out.push(AgentWarning::new(
            "container_without_image",
            "this agent declares no run.image, so there is nothing for Start to run. Set one, or \
             point the App tab at a dev_url.",
            true,
        )),
        Some(image) if source == "imported" && container::is_local_image(image) => {
            out.push(AgentWarning::new(
                "local_image_on_import",
                format!(
                    "the image '{image}' is local to the machine that built it; this install has \
                     to build or retag it before the agent can run"
                ),
                false,
            ));
        }
        Some(_) => {}
    }
    out
}

/// The **runtime half** (§4.3): what the box says right now, recomputed per
/// page load because it changes between two of them and must not be baked into
/// a stored row.
///
/// `image_absent_pull_never` is only raised when podman *answered*: "the image
/// is not there" and "podman could not say" are different facts, and the second
/// one is already `podman_unavailable`'s to report. The pull policy itself is
/// WP5; the field is read here so the warning is right when it lands.
pub async fn runtime_warnings(
    state: &SharedState,
    agent: &Agent,
    podman: &Result<(), String>,
) -> Vec<AgentWarning> {
    let mut out = Vec::new();
    // §5.1: a built-in the owner edited is never replaced behind their back, so
    // the fact that a newer manifest ships has to be *said* — beside the Reset
    // to shipped that adopts it.
    if seed::update_available(&agent.row) {
        out.push(AgentWarning::new(
            "builtin_update_available",
            "a newer built-in manifest ships with this version of lmgw. This row was edited (or \
             predates the hash lmgw records), so it is left exactly as it is — \"Reset to \
             shipped\" adopts the new one and keeps the config.",
            false,
        ));
    }
    let m = &agent.manifest;
    // §3.4: the package that was installed and the image the manifest runs are
    // two references, and an install does **not** rewrite the document — the
    // manifest's `run.image` is what every phase starts. When they disagree,
    // say so rather than letting the Runtime block's image and the provenance's
    // quietly differ on the same page.
    let prov = package::Provenance::of_row(&agent.row);
    if let (false, Some(image)) = (prov.image.is_empty(), m.image()) {
        if prov.image != image {
            out.push(AgentWarning::new(
                "install_image_mismatch",
                format!(
                    "this agent was installed from the image '{}', but its manifest names \
                     '{image}' — the manifest wins, so that is what a run, an apply and the app \
                     start. Re-import from image reads the package again; editing run.image on \
                     the Definition tab is the other way round.",
                    prov.image
                ),
                false,
            ));
        }
    }
    // §3.4's dev override is a *state*, not a fault — and one that changes what
    // the App tab is serving and makes an export non-portable, so the card and
    // the Run tab carry it beside everything else that is true right now.
    if let Some(url) = service::dev_url_of(agent) {
        out.push(AgentWarning::new(
            "dev_url_active",
            if service::service_of(agent).is_some() {
                format!(
                    "this agent's app is served from the dev server at {url}; no container is \
                     started for it. Runs and applies still use the image. Clear the dev_url to \
                     go back to the image."
                )
            } else {
                format!(
                    "this agent has a dev_url ({url}) but declares no run.service, so nothing \
                     reads it: there is no app to proxy. Clear it, or give the manifest a \
                     service block."
                )
            },
            false,
        ));
    }
    // ... and the override lmgw had to take away, which is the one state the
    // row cannot show by itself: the column is empty again, so without this the
    // App tab would quietly be back on the image and nobody would know why the
    // hot-reload loop stopped (§3.4, final review). The note carries the
    // refusal that dropped it, because there is more than one — a bind address
    // that moved onto the dev port, and a path a dev_url may no longer carry
    // (origins §4.7).
    else if let Some(cleared) = service::cleared_dev_urls(state).await.get(&agent.row.id) {
        let url = &cleared.url;
        out.push(AgentWarning::new(
            "dev_url_cleared",
            if cleared.why.is_empty() {
                format!(
                    "the dev server override on this agent ({url}) was cleared: it is no longer \
                     a legal dev_url for this gateway. Set a new one to go back to the \
                     hot-reload loop; the app is served from the image until then."
                )
            } else {
                format!(
                    "the dev server override on this agent ({url}) was cleared because it is no \
                     longer a legal dev_url for this gateway: {}. Set a new one to go back to \
                     the hot-reload loop; the app is served from the image until then.",
                    cleared.why
                )
            },
            false,
        ));
    }
    // A script step runs in a container too, so the podman half below is its
    // gate as well as an image agent's.
    let scripted = m.apply_step().is_some_and(manifest::Step::is_script);
    if m.kind() != "container" && !scripted {
        return out;
    }
    // The existing required rule, under a name that says what to do about it
    // (mounts §5.9): a required mount with nothing in it fails
    // `validate_values` at the click with "'notes' is required", which is true
    // and tells nobody where the folder is chosen.
    let bound = agent.config_values();
    for f in m.mount_fields().filter(|f| f.required) {
        let set = bound
            .get(&f.name)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        if !set {
            out.push(AgentWarning::new(
                "mount_unbound",
                format!(
                    "this agent needs a {} for '{}' and none is bound: bind '{}' on the Run tab",
                    f.kind.as_str(),
                    f.name,
                    f.name
                ),
                true,
            ));
        }
    }
    if let Err(e) = podman {
        // Blocking for a container agent, whose every phase is a container. A
        // batch agent with a `script` apply step is the other case: its list
        // and classify phases need nothing from podman, so disabling Start
        // outright would take away the half that still works. The apply run
        // itself fails with this same sentence when it is actually pressed.
        out.push(AgentWarning::new(
            "podman_unavailable",
            if scripted && m.kind() != "container" {
                format!(
                    "the apply step is a script, which runs in a container, and podman could not \
                     be run on this box: {e}. Listing and classifying still work; Apply will fail."
                )
            } else {
                format!(
                    "podman is required for container agents and could not be run on this box: {e}"
                )
            },
            m.kind() == "container",
        ));
        return out;
    }
    if m.pull() == manifest::PullPolicy::Never {
        if let Some(image) = m.image() {
            if container::image_present(state, image).await == container::ImagePresence::Absent {
                out.push(AgentWarning::new(
                    "image_absent_pull_never",
                    format!(
                        "the image '{image}' is not on this box and run.pull is 'never', so Start \
                         would fail rather than download it. Pull it, or set run.pull."
                    ),
                    true,
                ));
            }
        }
    }
    // §6.2: "your tokens are being written to persistent storage" is not a
    // `tracing::warn!`-grade fact.
    let prefix = state.snapshot().settings.container_prefix.clone();
    let (root, fallback) = container::runs_root(&state.data_dir, &prefix);
    if fallback {
        out.push(AgentWarning::new(
            "secrets_dir_fallback",
            format!(
                "XDG_RUNTIME_DIR is unset, so each run's secrets.json is written under {} — \
                 persistent storage — instead of a host tmpfs",
                root.display()
            ),
            false,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// The tools one agent's token may call (§3.1)
// ---------------------------------------------------------------------------

/// What one agent's token may reach: the labels its manifest's `tools[]`
/// names, and the exposed tool names those resolve to right now.
#[derive(Debug, Clone, Default)]
pub struct ToolGrant {
    /// The labels, as written — what decides whether a server is worth waking
    /// for this agent before its tools can be listed at all.
    pub labels: Vec<String>,
    /// The allow list proper, resolved against the surface as it is now.
    pub names: Vec<String>,
}

/// The agent's allow list, resolved against the surface as it is now.
///
/// An agent whose row is gone or whose manifest this build cannot read gets an
/// **empty** grant from the caller rather than an unfiltered one: the manifest
/// *is* the allow list, so no manifest is no tools. The caller turns that into
/// the same named refusal any other unlisted name gets.
pub async fn tool_grant(state: &SharedState, agent_id: &str) -> Result<ToolGrant, String> {
    let row = crate::store::get_agent(&state.db, agent_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no agent with id '{agent_id}'"))?;
    let agent = Agent::from_row(row)?;
    Ok(ToolGrant {
        labels: agent
            .manifest
            .tools
            .iter()
            .map(|t| t.label.clone())
            .collect(),
        names: ToolSurface::load(state)
            .await
            .allowed(&agent.manifest.tools),
    })
}

// ---------------------------------------------------------------------------
// Cost per run, for callers lmgw does not drive (§3.1)
// ---------------------------------------------------------------------------

/// What one run spent on traffic that arrived over HTTP carrying
/// `X-Lmgw-Run: <job id>` — a container's own `/v1` and `/mcp` calls.
///
/// A request that arrives from outside has already been priced by
/// [`crate::proxy`] before anyone knows which run it belongs to, so these
/// totals are the **sum of its rows' costs**, by the rollup's own rule for an
/// unpriced row ([`crate::store::NewRequestLog::row_cost`]) — the rule the
/// in-process executor's meter sums its calls' rows by, so neither reads a
/// run lower than its rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunTotals {
    /// The tokens the run's rows reported, a failed call's included.
    pub usage: crate::ir::Usage,
    /// What the rows add up to: nothing until a row adds to it, unknown for
    /// good once one was unpriced work — never the priced rows' partial sum.
    pub cost: crate::pricing::CostTotal,
    /// Calls that reached an upstream and came back without an error: a
    /// refused, failed or stopped call is none, though its row's cost counts.
    pub model_calls: u32,
    pub tool_calls: u32,
}

impl RunTotals {
    /// `None` is "nobody could price this", which is not zero: no row added
    /// to the total, or one that could not be priced.
    pub fn cost_micro(&self) -> Option<i64> {
        self.cost.micro()
    }
}

/// Per-run totals for every run currently open, keyed by job id.
///
/// On [`crate::state::AppState`] rather than in a `static` for the same reason
/// [`batch::RunBuffer`] is: two gateways in one test process must not see each
/// other's runs through colliding job ids.
///
/// A run is open from [`Self::open`] — when its job goes live, before any
/// request can name it — until [`Self::take`]. A call noted for a run that
/// is not open is dropped: long-lived stamped work (a realtime session, a
/// long `/v1/responses` loop) that finishes after its run was read and
/// forgotten re-creates no entry nobody would ever take.
#[derive(Default)]
pub struct RunMeters(std::sync::Mutex<std::collections::HashMap<i64, RunTotals>>);

impl RunMeters {
    /// Open a run's meter, empty. Its job is about to go live.
    pub fn open(&self, run: i64) {
        self.0.lock().unwrap().entry(run).or_default();
    }

    /// Fold one finished call's row into the run it stamped itself with:
    /// its tokens, what it adds to the cost (`row`, the row's own
    /// [`crate::store::NewRequestLog::row_cost`]), and — `answered`, it came
    /// back without an error — one model call.
    ///
    /// The first row that adds to the cost starts the total and every later
    /// one adds to it. Unpriced work makes the total unknown for the rest of
    /// the run: a sum of the others would read as the whole run's cost.
    pub fn note_row(
        &self,
        run: i64,
        usage: &crate::ir::Usage,
        row: crate::pricing::RowCost,
        answered: bool,
    ) {
        let mut m = self.0.lock().unwrap();
        let Some(t) = m.get_mut(&run) else {
            return;
        };
        t.usage.add(usage);
        t.cost.add(row);
        t.model_calls += u32::from(answered);
    }

    /// Same for a tool call — a `tools/call` on `/mcp`, or one lmgw ran for a
    /// stamped `/v1/responses` or realtime turn. A tool call has no usage of
    /// its own — `counts_in_token_stats` already excludes the `"mcp"` proto —
    /// so only the count moves.
    pub fn note_tool_call(&self, run: i64) {
        if let Some(t) = self.0.lock().unwrap().get_mut(&run) {
            t.tool_calls += 1;
        }
    }

    /// Read without disturbing, for a run still going.
    pub fn read(&self, run: i64) -> RunTotals {
        self.0
            .lock()
            .unwrap()
            .get(&run)
            .cloned()
            .unwrap_or_default()
    }

    /// Read and forget, when the run ends: the run is closed, and a call
    /// noted for it from here on is dropped.
    pub fn take(&self, run: i64) -> RunTotals {
        self.0.lock().unwrap().remove(&run).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest half of §4.3 is a **pure function of the document** — no
    /// gateway, no podman, no row beyond its `source`. That split is not
    /// decoration: it is what lets the import report carry these warnings
    /// before the agent exists, and what keeps a fact that changes between two
    /// page loads out of a stored row.
    fn doc(run: &str) -> Manifest {
        manifest::load(&format!(
            r#"{{ "schema_version": 1, "id": "a", "name": "A",
                  "model": {{ "alias": "m" }}, "run": {run} }}"#
        ))
        .expect("the fixture manifest parses")
    }

    fn codes(m: &Manifest, source: &str) -> Vec<(&'static str, bool)> {
        manifest_warnings(m, source)
            .into_iter()
            .map(|w| (w.code, w.blocks_start))
            .collect()
    }

    #[test]
    fn a_container_with_no_image_blocks_start_and_says_so() {
        let m = doc(r#"{ "kind": "container", "service": { "port": 8080 } }"#);
        assert_eq!(codes(&m, "authored"), [("container_without_image", true)]);
        let w = &manifest_warnings(&m, "authored")[0];
        assert!(w.message.contains("run.image"), "{}", w.message);
    }

    #[test]
    fn a_local_image_warns_on_import_and_never_blocks_start() {
        // Exporting to the same box is the common case, so this is a notice,
        // not a refusal (§3.4).
        let m = doc(r#"{ "kind": "container", "image": "localhost/a:1" }"#);
        assert_eq!(codes(&m, "imported"), [("local_image_on_import", false)]);
        // Authored here, so there is nothing to warn about: the owner built it.
        assert!(codes(&m, "authored").is_empty());
    }

    /// A minimal batch manifest with whatever apply step the caller wants.
    fn batch_with(apply: &str) -> Manifest {
        manifest::load(&format!(
            r#"{{ "schema_version": 1, "id": "a", "name": "A",
                  "model": {{ "alias": "m" }},
                  "run": {{ "kind": "batch",
                    "source": {{ "tool": "t" }},
                    "item": {{ "id": "{{{{item.id}}}}" }},
                    "apply": {apply} }} }}"#
        ))
        .expect("the fixture manifest parses")
    }

    /// §4.3: `apply.turn` is a **warning that blocks Start**, not a parse
    /// error. The manifest loads, the card says why, and the Definition editor
    /// stays reachable.
    #[test]
    fn an_apply_turn_warns_and_blocks_start_without_refusing_the_manifest() {
        let m = batch_with(r#"{ "turn": { "prompt": "write it" } }"#);
        assert_eq!(codes(&m, "builtin"), [("apply_turn", true)]);
        let w = &manifest_warnings(&m, "builtin")[0];
        assert!(
            w.message.contains("apply may not run a model turn"),
            "{}",
            w.message
        );
        assert!(w.message.contains("`script`"), "{}", w.message);
    }

    /// A script with no declared `output` stores whatever it returns. That is
    /// legal and visible, never a refusal — so the warning does not block.
    #[test]
    fn a_script_without_an_output_schema_warns_but_does_not_block_start() {
        let m = batch_with(r#"{ "script": "export async function apply(){}" }"#);
        assert_eq!(codes(&m, "builtin"), [("script_without_output", false)]);
        // Declared, and there is nothing left to say.
        let ok = batch_with(
            r#"{ "script": "export async function apply(){}",
                 "output": { "type": "object", "properties": { "applied": { "type": "integer" } } } }"#,
        );
        assert!(codes(&ok, "builtin").is_empty());
        // A direct tool call is the third shape and warns about neither.
        assert!(codes(&batch_with(r#"{ "tool": "t" }"#), "builtin").is_empty());
    }

    /// The Start gate is `requires_ok && no blocking warning`, and the two
    /// step warnings sit on opposite sides of it.
    #[test]
    fn the_start_gate_reads_blocks_start_and_nothing_else() {
        let blocking = |m: &Manifest| {
            manifest_warnings(m, "builtin")
                .iter()
                .any(|w| w.blocks_start)
        };
        assert!(blocking(&batch_with(r#"{ "turn": { "prompt": "p" } }"#)));
        assert!(!blocking(&batch_with(
            r#"{ "script": "export async function apply(){}" }"#
        )));
    }

    /// A mount field is a slot in a container's filesystem (mounts §5.1), so
    /// on a manifest with no container it is a blocking warning rather than a
    /// form that asks for a folder nothing will ever bind.
    #[test]
    fn a_mount_field_on_a_manifest_with_no_container_blocks_start() {
        let with_mount = |kind: &str| {
            manifest::load(&format!(
                r#"{{ "schema_version": 1, "id": "a", "name": "A",
                      "model": {{ "alias": "m" }},
                      "config": {{ "schema": {{ "type": "object", "properties": {{
                        "notes": {{ "type": "string", "format": "directory" }},
                        "key": {{ "type": "string", "format": "file" }}
                      }} }} }},
                      "run": {kind} }}"#
            ))
            .expect("the fixture manifest parses")
        };
        let m = with_mount(r#"{ "kind": "chat" }"#);
        assert_eq!(
            codes(&m, "authored"),
            [
                ("mount_field_without_container", true),
                ("mount_field_without_container", true)
            ]
        );
        let w = manifest_warnings(&m, "authored");
        assert!(
            w[0].message
                .contains("'notes' is a directory field, but this agent has no container"),
            "{}",
            w[0].message
        );
        assert!(
            w[1].message.contains("'key' is a file field"),
            "{}",
            w[1].message
        );

        // A **batch** manifest is the second kind with no container of its
        // own, and it is the one an owner is likelier to write a mount field
        // on by hand — its steps run here, on this desk, and a folder they
        // could read would look reasonable. It is not: nothing binds it.
        let m = with_mount(
            r#"{ "kind": "batch", "source": { "tool": "t" },
                 "item": { "id": "{{item.id}}" } }"#,
        );
        assert_eq!(
            codes(&m, "authored"),
            [
                ("mount_field_without_container", true),
                ("mount_field_without_container", true)
            ]
        );
        assert!(
            manifest_warnings(&m, "authored")[0]
                .message
                .contains("this agent has no container to mount it into"),
            "{:?}",
            manifest_warnings(&m, "authored")[0]
        );

        // On a container manifest it is exactly what it says it is.
        let m = with_mount(r#"{ "kind": "container", "image": "ghcr.io/acme/a:1" }"#);
        assert!(codes(&m, "authored").is_empty());
    }

    #[test]
    fn a_registry_image_and_every_other_run_kind_warn_about_nothing() {
        assert!(codes(
            &doc(r#"{ "kind": "container", "image": "ghcr.io/acme/a:1" }"#),
            "imported"
        )
        .is_empty());
        assert!(codes(&doc(r#"{ "kind": "chat" }"#), "imported").is_empty());
    }
}
