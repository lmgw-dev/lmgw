//! The agent manifest (agent-catalog design §2): types, schema-v1 validation
//! and the config-schema subset the Run tab's form is drawn from.
//!
//! An agent *is* this document. Nothing it needs is compiled into the gateway:
//! it names a model, prompts, the MCP tool labels it may reach, a config form
//! and a run shape, and that is the whole vocabulary. If a manifest cannot
//! express something, an MCP server does the logic (§1, principle 1).
//!
//! **Unknown fields are refused everywhere** (`deny_unknown_fields`), so a typo
//! is an error naming the key rather than a silently ignored step. That is the
//! single most important property here: a manifest that half-works is worse
//! than one that is rejected, because the half that was ignored is the half
//! nobody notices until a run writes the wrong thing.
//!
//! **A manifest never carries a secret** (§1, principle 2). Credentials belong
//! to the MCP server registration or to a `secret` config field, which is
//! write-only and never exported.
//!
//! Template rendering lives next door in [`super::template`]; this module owns
//! the compile-time half of it — every `{{config.<field>}}` in the document is
//! checked against the config schema at save and at import.

use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use super::template::{self, Root};

/// The only manifest version this build understands. An unknown version is
/// refused at import **with the version named** (§2.1), rather than being
/// deserialized on a hope and failing on a field that moved.
pub const SCHEMA_VERSION: u64 = 1;

/// Path segments `/api/agents/…` owns itself, so an agent cannot be given an id
/// that the route table would shadow.
const RESERVED_IDS: [&str; 3] = ["runs", "import", "new"];

// ---------------------------------------------------------------------------
// An order-preserving string map
// ---------------------------------------------------------------------------

/// A JSON object whose key order survives a round trip.
///
/// `serde_json::Map` is a `BTreeMap` in this build, which would silently
/// alphabetize a config form's fields and a review table's columns — the author
/// of a manifest put "model" before "categories" on purpose. Duplicate keys are
/// refused rather than last-wins, because a duplicate in a hand-written
/// manifest is a mistake worth naming.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OrderedMap<V>(Vec<(String, V)>);

impl<V> OrderedMap<V> {
    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.0.iter().map(|(k, v)| (k, v))
    }
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.iter().map(|(k, _)| k)
    }
    pub fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl<V: Serialize> Serialize for OrderedMap<V> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedMap<V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V0<V>(std::marker::PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for V0<V> {
            type Value = OrderedMap<V>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut out: Vec<(String, V)> = Vec::new();
                while let Some((k, v)) = a.next_entry::<String, V>()? {
                    if out.iter().any(|(existing, _)| *existing == k) {
                        return Err(de::Error::custom(format!("duplicate key '{k}'")));
                    }
                    out.push((k, v));
                }
                Ok(OrderedMap(out))
            }
        }
        d.deserialize_map(V0(std::marker::PhantomData))
    }
}

// ---------------------------------------------------------------------------
// §2.1 — the manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u64,
    /// `[a-z0-9][a-z0-9-]{0,63}`. The catalog key, the export filename, the
    /// jobs key.
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Free text, informational; shown on the card and in the export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub model: ModelSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolRef>,
    pub run: RunSpec,
}

/// The IR [`Params`](crate::ir::Params) subset a manifest may set.
///
/// **No `max_tokens`** (§2.1): the model's context length is known exactly from
/// the model selection, and a run uses it. A guessed ceiling here would be an
/// invisible cap on every answer the agent produces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    /// Usually `{{config.model}}`, so the picker on the Run tab decides.
    pub alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<crate::ir::ReasoningControl>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigBlock {
    pub schema: ConfigSchema,
}

/// One entry of `tools[]`: a label from the MCP plane, optionally narrowed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRef {
    /// A registered server's label, or the built-in `lmgw` / `docs`.
    pub label: String,
    /// Narrows to these exposed tool names; absent means the whole surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<String>>,
    /// An import hint (§5). Documentation carried with the manifest — lmgw
    /// never fetches anything from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<Install>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Install {
    pub kind: InstallKind,
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    Git,
    Image,
    Url,
}

// ---------------------------------------------------------------------------
// §2.2 — steps
// ---------------------------------------------------------------------------

/// Where an agent touches the outside world: a **direct call** (`tool` + `args`),
/// a **turn** (the model drives the existing tool loop) or a **script**
/// (deterministic JS in a stock Node container — container-runtime §4.2).
///
/// Modelled as one struct with the three shapes optional, rather than an
/// untagged enum, so a malformed step is reported as "a step is either … or …"
/// naming what was found instead of serde's "data did not match any variant".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Exposed tool name for a direct call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Templated arguments for the direct call (§2.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<Turn>,
    /// An ES module exporting one function per phase — `apply(ctx)` for the
    /// apply step (container-runtime §4.2). Run by the shim in
    /// `Settings.agent_script_image`; not a second runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<Script>,
    /// The schema the script's return value is validated against, exactly as
    /// [`Turn::output`] is for a turn. Belongs to `script` alone: a turn
    /// carries its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

impl Step {
    pub fn is_turn(&self) -> bool {
        self.turn.is_some()
    }

    pub fn is_script(&self) -> bool {
        self.script.is_some()
    }
}

/// A `script` as the manifest carries it: one string, or an array of lines.
///
/// Both forms are kept as written rather than normalized on the way in, so
/// [`Manifest::to_json`] round-trips a hand-edited manifest byte for byte. The
/// array form is what survives editing a manifest in the Definition tab
/// without a wall of `\n`; [`Self::text`] is the module either one becomes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Script {
    Text(String),
    Lines(Vec<String>),
}

impl Script {
    /// The module text: the string as-is, or the lines joined with `\n`.
    pub fn text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Lines(lines) => lines.join("\n"),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text().trim().is_empty()
    }
}

/// The model runs the tool loop with exactly `tools` attached (§2.2/§4.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub prompt: String,
    /// Structured result, enforced in two stages (§2.2): parse the final answer
    /// against it, else **one** further call with no tools and a
    /// `response_format` carrying it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

// ---------------------------------------------------------------------------
// §2.4 / §2.5 — the run kinds
// ---------------------------------------------------------------------------

/// `#[allow(large_enum_variant)]`: the variants are deliberately lopsided —
/// a `chat` preset is one string and a `batch` run is the whole pipeline — and
/// boxing the big one would turn the serde shape into a newtype variant for a
/// saving that buys nothing. A `RunSpec` is parsed once per API call and held
/// one at a time, never in a collection on any path that matters.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunSpec {
    /// A Chat-thread preset; needs no new runtime (§2.5).
    Chat {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system: Option<String>,
    },
    /// The agent's logic runs in a Podman container the owner built
    /// (container-runtime §4.1). lmgw owns identity, money, logs, secrets and
    /// the shell; the image owns what the agent actually does.
    Container {
        /// OCI reference. Required unless the manifest declares a `service`
        /// and the row carries a `dev_url`. `localhost/…` is accepted and
        /// flagged on export.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<String>,
        /// Shown, never implicit: an image that is not on the box must not
        /// turn a Start into a download nobody asked for.
        #[serde(default)]
        pull: PullPolicy,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entrypoint: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Review-table header order. Declared here because `Row::columns` is
        /// a map and this build's maps are alphabetical — the same reason
        /// [`super::batch::review_columns`] exists.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        columns: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        review: Option<Review>,
        /// Which phases this image implements. `["run", "apply"]` is the
        /// review-gated shape.
        #[serde(default = "default_phases", skip_serializing_if = "Vec::is_empty")]
        phases: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limits: Option<Limits>,
        /// Absent = no service mode, no proxy route, no App tab (§3.3). Parsed
        /// and validated here; served by WP4.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service: Option<Service>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provides: Option<Provides>,
        /// **Per phase**: `{"run": <schema>, "apply": <schema>}`. The schema
        /// the named phase's `output` event is validated against at close
        /// (§3.2). A phase with no key here is unvalidated, and the Run tab
        /// says so — one schema shared by both phases would be unusable, since
        /// a run reports rows and an apply reports what it wrote.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<OrderedMap<Value>>,
    },
    /// The mail workflow's shape, generalized (§2.4).
    Batch {
        /// Runs once; must yield an array of objects.
        source: Step,
        /// JSON pointer into the source result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        items_path: Option<String>,
        item: BatchItem,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        review: Option<Review>,
        /// Receives `rows`. The only stage that writes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        apply: Option<Step>,
        /// The bounds a run is under (container-runtime §4.1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limits: Option<Limits>,
    },
}

impl RunSpec {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Chat { .. } => "chat",
            Self::Batch { .. } => "batch",
            Self::Container { .. } => "container",
        }
    }
}

/// `run.pull` (§3.4, §4.1): **default `never`**, for the reason
/// [`Registry::run_throwaway`](crate::runtime::registry::Registry::run_throwaway)
/// spells out — an image that is not on the box must not turn an install or a
/// Start into a multi-gigabyte download nobody asked for. `missing` and
/// `always` are selectable and printed next to the image field, so the choice
/// is never implicit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullPolicy {
    #[default]
    Never,
    Missing,
    Always,
}

impl PullPolicy {
    pub const ALL: [PullPolicy; 3] = [Self::Never, Self::Missing, Self::Always];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Missing => "missing",
            Self::Always => "always",
        }
    }

    /// The op argument's three words, and nothing else: an unrecognised policy
    /// is refused naming the three rather than falling back to a default the
    /// caller did not choose.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

/// Service mode's declaration (§3.3). Parsed and validated by WP2 so a
/// manifest that carries it is readable; the proxy, the on-demand start and
/// the idle sweep are WP4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// In-container port. The host side is ephemeral, per start: lmgw publishes
    /// it on `127.0.0.1:<ephemeral>` and records which port in the run log and
    /// in `AgentDetail.service`.
    pub port: u32,
    /// What the start probes for. Default `/`; **empty** means a TCP connect to
    /// [`port`](Self::port) instead of an HTTP GET.
    #[serde(default = "default_health_path")]
    pub health_path: String,
    /// `0` = never idle-stop, exactly as `McpServer::idle_seconds` means it.
    #[serde(default = "default_idle_seconds")]
    pub idle_seconds: i64,
    /// How long the first request waits for the container to answer its health
    /// probe. **`0` = no limit**: the request waits as long as the container
    /// takes, the same reading `0` has for every other bound in §4.1.
    #[serde(default = "default_start_timeout_seconds")]
    pub start_timeout_seconds: u64,
}

fn default_health_path() -> String {
    "/".to_string()
}
fn default_idle_seconds() -> i64 {
    300
}
fn default_start_timeout_seconds() -> u64 {
    30
}

/// What the agent's own container serves back to lmgw (§3.3). Requires
/// `service`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provides {
    /// Path inside the container that speaks MCP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<String>,
}

/// The limits' defaults, in one place so the Run tab, the runner, the ledger
/// and the error messages that name them cannot drift (container-runtime §4.1).
pub const DEFAULT_DEADLINE_SECONDS: u64 = 600;
pub const DEFAULT_MEMORY_MB: u64 = 512;
pub const DEFAULT_CPUS: f64 = 2.0;
pub const DEFAULT_PIDS: u64 = 256;
pub const DEFAULT_STOP_GRACE_SECONDS: u64 = 10;

/// The phases a container implements when it does not say (§4.1).
fn default_phases() -> Vec<String> {
    vec!["run".to_string()]
}

/// The bounds a run is under — visible manifest fields, with their defaults
/// printed rather than applied behind the owner's back (container-runtime
/// §4.1).
///
/// **`0` always means "no limit"**, never a hidden fallback: the flag is
/// omitted from the `podman run` argv entirely, and the Run tab prints
/// "unlimited" rather than a number lmgw invented. The single exception is
/// [`Self::stop_grace_seconds`], where `0` is *stricter* — SIGKILL at once,
/// with no chance to flush — and the Run tab spells that out instead of
/// calling it unlimited.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Limits {
    /// `--memory <n>m`; `0` = no cgroup memory limit.
    pub memory_mb: u64,
    /// `--cpus <f>`; `0` = no CPU quota.
    pub cpus: f64,
    /// `--pids-limit <n>`; `0` = no PID limit.
    pub pids: u64,
    /// Wall clock for one run, counted from the container's start for a run
    /// lmgw started and from the open for a ledger run. `0` = unbounded: the
    /// run ends when the container (or whatever opened it) does.
    pub deadline_seconds: u64,
    /// `podman stop -t <n>`. `0` is **not** "no limit": it is no grace at all.
    pub stop_grace_seconds: u64,
    /// `--read-only --tmpfs /tmp`. No size on the tmpfs: its pages are charged
    /// to the container's memory cgroup, so `memory_mb` already bounds it.
    pub read_only: bool,
}

/// Deserialized by hand so a bad bound is refused **with the field named**.
///
/// `#[derive(Deserialize)]` over `u64` answers a negative with *"invalid value:
/// integer `-1`, expected u64"* and no field at all, which is the one error
/// message in this document that must not be vague: a limit is a promise the
/// owner is making to themselves, and one they cannot locate is one they cannot
/// fix. Reading each value as a `serde_json::Value` first costs a few lines and
/// buys "run.limits.memory_mb is -1; a limit is 0 (no limit) or above".
impl<'de> Deserialize<'de> for Limits {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            memory_mb: Option<Value>,
            cpus: Option<Value>,
            pids: Option<Value>,
            deadline_seconds: Option<Value>,
            stop_grace_seconds: Option<Value>,
            read_only: Option<bool>,
        }
        let raw = Raw::deserialize(d)?;
        let count = |v: Option<Value>, at: &str, default: u64| -> Result<u64, D::Error> {
            let Some(v) = v else { return Ok(default) };
            match v.as_u64() {
                Some(n) => Ok(n),
                None => Err(de::Error::custom(format!(
                    "run.limits.{at} is {v}; a limit is a whole number, 0 (no limit) or above"
                ))),
            }
        };
        let cpus = match raw.cpus {
            None => DEFAULT_CPUS,
            Some(v) => match v.as_f64() {
                Some(n) if n >= 0.0 => n,
                _ => {
                    return Err(de::Error::custom(format!(
                        "run.limits.cpus is {v}; a limit is 0 (no CPU quota) or above"
                    )))
                }
            },
        };
        Ok(Self {
            memory_mb: count(raw.memory_mb, "memory_mb", DEFAULT_MEMORY_MB)?,
            cpus,
            pids: count(raw.pids, "pids", DEFAULT_PIDS)?,
            deadline_seconds: count(
                raw.deadline_seconds,
                "deadline_seconds",
                DEFAULT_DEADLINE_SECONDS,
            )?,
            stop_grace_seconds: count(
                raw.stop_grace_seconds,
                "stop_grace_seconds",
                DEFAULT_STOP_GRACE_SECONDS,
            )?,
            read_only: raw.read_only.unwrap_or_else(default_read_only),
        })
    }
}

fn default_read_only() -> bool {
    true
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_mb: DEFAULT_MEMORY_MB,
            cpus: DEFAULT_CPUS,
            pids: DEFAULT_PIDS,
            deadline_seconds: DEFAULT_DEADLINE_SECONDS,
            stop_grace_seconds: DEFAULT_STOP_GRACE_SECONDS,
            read_only: default_read_only(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchItem {
    /// Stable row identity; required. An item whose `id` renders empty marks
    /// the row errored (§2.3).
    pub id: String,
    /// Optional; its result is `fetched`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch: Option<Step>,
    /// Review-table columns, in author order.
    #[serde(default, skip_serializing_if = "OrderedMap::is_empty")]
    pub columns: OrderedMap<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ItemOutput>,
    /// Parallel classify calls. A template or a literal integer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<Value>,
}

/// The per-item structured output, in its two forms (§2.4).
///
/// One struct rather than an untagged enum for the same reason [`Step`] is: the
/// error has to name the field that is missing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemOutput {
    /// `enum_from` form: the single field the model answers with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// `config.<field>` naming the array of strings the enum is built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enum_from: Option<String>,
    /// General form: an object schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    /// The answer a failed or unconvinced call gets; also what marks a row as
    /// needing attention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Value>,
}

impl ItemOutput {
    pub fn is_enum_form(&self) -> bool {
        self.enum_from.is_some()
    }

    /// The enum the classify call is constrained to, built from the *stored*
    /// config so widening it is a config edit and not a manifest edit (§2.4,
    /// "re-run attention rows").
    ///
    /// The fallback is appended **exactly once and last**: it is the value the
    /// review table sorts into "needs attention", so a duplicate or a
    /// mid-position occurrence would make an ordinary answer look like a
    /// failure.
    pub fn enum_values(&self, config: &Value) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(path) = &self.enum_from {
            if let Some(Value::Array(items)) = lookup_path(config, path) {
                for item in items {
                    let s = match item {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if !s.is_empty() && !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
        }
        if let Some(Value::String(fb)) = &self.fallback {
            out.retain(|v| v != fb);
            out.push(fb.clone());
        }
        out
    }

    /// The JSON schema handed to the model as `response_format` (§4.2). The
    /// `enum_from` form builds a one-field object around [`Self::enum_values`].
    pub fn response_schema(&self, config: &Value) -> Value {
        if let (Some(field), true) = (&self.field, self.is_enum_form()) {
            return serde_json::json!({
                "type": "object",
                "properties": { field.as_str(): { "type": "string", "enum": self.enum_values(config) } },
                "required": [field.as_str()],
                "additionalProperties": false,
            });
        }
        self.schema.clone().unwrap_or(Value::Null)
    }
}

/// `config.categories` → the value, against a plain config object.
fn lookup_path<'a>(config: &'a Value, path: &str) -> Option<&'a Value> {
    let rest = path.strip_prefix("config.")?;
    let mut cur = config;
    for seg in rest.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    /// Output fields the reviewer may override before Apply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editable: Vec<String>,
}

// ---------------------------------------------------------------------------
// §2.6 — the config schema subset
// ---------------------------------------------------------------------------

/// The config form's schema. Properties stay raw [`Value`]s until
/// [`Self::fields`] converts them, so a property outside the subset is reported
/// as `config.schema.properties.<name>: …` rather than as an unlocated serde
/// error.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSchema {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default, skip_serializing_if = "OrderedMap::is_empty")]
    pub properties: OrderedMap<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Integer,
    Number,
    Boolean,
    /// Of `string` only — the mail categories.
    Array,
}

impl FieldType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Array => "array",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "string" => Self::String,
            "integer" => Self::Integer,
            "number" => Self::Number,
            "boolean" => Self::Boolean,
            "array" => Self::Array,
            _ => return None,
        })
    }
}

/// The string formats the form renderer understands (§2.6, mounts §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// Write-only: never returned by the API, never exported.
    Secret,
    /// A picker over `/v1/models`.
    ModelAlias,
    /// A textarea.
    Multiline,
    /// A host directory the owner binds into the container (mounts §5.1).
    Directory,
    /// A single host file, same deal.
    File,
}

/// The formats a manifest may name, for the sentence an unknown one earns.
const FORMAT_NAMES: &str = "secret, model_alias, multiline, directory, file";

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Secret => "secret",
            Self::ModelAlias => "model_alias",
            Self::Multiline => "multiline",
            Self::Directory => "directory",
            Self::File => "file",
        }
    }
    /// The format this name spells, or `None` for a name no manifest may use.
    ///
    /// Public because a reader that has a *rendered* field — the detail DTO's
    /// `format` string, a raw property of a manifest this build cannot parse —
    /// still has to ask [`Self::mount_kind`] rather than re-deriving which
    /// formats count (mounts §5.1).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "secret" => Self::Secret,
            "model_alias" => Self::ModelAlias,
            "multiline" => Self::Multiline,
            "directory" => Self::Directory,
            "file" => Self::File,
            _ => return None,
        })
    }
    /// The two formats that name a **slot** rather than a value (§5.1).
    pub fn mount_kind(self) -> Option<MountKind> {
        match self {
            Self::Directory => Some(MountKind::Directory),
            Self::File => Some(MountKind::File),
            _ => None,
        }
    }
}

/// What a mount field names on the host: a directory, or one regular file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountKind {
    Directory,
    File,
}

impl MountKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
        }
    }
}

/// How the container may open a bound mount (§5.1).
///
/// **The manifest declares it, not the form**: the agent is what knows whether
/// it writes, and an owner asked to choose would be guessing on the image's
/// behalf. `ro` is the default, so a manifest that says nothing asks for the
/// narrower of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    #[default]
    Ro,
    Rw,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ro => "ro",
            Self::Rw => "rw",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ro" => Self::Ro,
            "rw" => Self::Rw,
            _ => return None,
        })
    }
}

/// One mount **slot** a manifest declares: what a run has to bind, and how.
///
/// The query the runtime iterates (§5.5, §5.6) and the storage path checks
/// against (§5.3) — derived from [`Field`], so there is one definition of what
/// makes a field a mount field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountField {
    pub name: String,
    pub kind: MountKind,
    pub access: Access,
    pub required: bool,
}

impl MountField {
    /// `/lmgw/mounts/<field>` — what the container sees, and the only path any
    /// reader but the owner is ever given (§5.2).
    pub fn inside(&self) -> String {
        mount_inside(&self.name)
    }
}

/// `/lmgw/mounts/<field>` from the field name alone.
///
/// The one spelling of the container side, for the readers that hold a name
/// and no schema: the agent's own view of a row whose manifest this build
/// cannot parse (principals §3.10) goes through here rather than through a
/// second `format!` that could drift from [`MountField::inside`].
pub fn mount_inside(field: &str) -> String {
    format!("/lmgw/mounts/{field}")
}

/// One config field, converted out of the raw schema and ready to render.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: FieldType,
    pub format: Option<Format>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub default: Option<Value>,
    pub enum_values: Vec<String>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub required: bool,
    /// Only a mount field carries one, and only the manifest sets it (§5.1).
    pub access: Access,
}

impl Field {
    pub fn is_secret(&self) -> bool {
        self.format == Some(Format::Secret)
    }
    /// This field as a mount slot, or `None` because it names a value.
    pub fn mount(&self) -> Option<MountField> {
        Some(MountField {
            name: self.name.clone(),
            kind: self.format?.mount_kind()?,
            access: self.access,
            required: self.required,
        })
    }
    pub fn is_mount(&self) -> bool {
        self.format.and_then(Format::mount_kind).is_some()
    }
}

/// The raw shape of one property, closed so anything outside the subset is a
/// named error rather than a silently dropped keyword.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    #[serde(rename = "type")]
    ty: String,
    title: Option<String>,
    description: Option<String>,
    default: Option<Value>,
    format: Option<String>,
    /// `ro` | `rw`, and only on a mount format (§5.1).
    access: Option<String>,
    #[serde(rename = "enum")]
    enum_values: Option<Vec<Value>>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    items: Option<Value>,
}

/// `items` of an array field. `title`/`description` are accepted and ignored —
/// they are legal JSON Schema and the renderer has no place for them, which is
/// not a reason to refuse the document.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawItems {
    #[serde(rename = "type")]
    ty: String,
    #[allow(dead_code)]
    title: Option<String>,
    #[allow(dead_code)]
    description: Option<String>,
}

impl ConfigSchema {
    /// Convert and check every property against §2.6.
    ///
    /// `Err` carries **all** the problems, not the first: a pasted manifest with
    /// three bad fields should report three.
    pub fn fields(&self) -> Result<Vec<Field>, Vec<String>> {
        let mut out = Vec::new();
        let mut errors = Vec::new();
        if self.ty != "object" {
            errors.push(format!(
                "config.schema.type is '{}'; a config schema is an object",
                self.ty
            ));
        }
        for (name, raw) in self.properties.iter() {
            let at = format!("config.schema.properties.{name}");
            let f: RawField = match serde_json::from_value(raw.clone()) {
                Ok(f) => f,
                Err(e) => {
                    errors.push(format!("{at}: {e}"));
                    continue;
                }
            };
            let Some(ty) = FieldType::parse(&f.ty) else {
                errors.push(format!(
                    "{at}: unknown type '{}' (string, integer, number, boolean, array)",
                    f.ty
                ));
                continue;
            };
            let mut format = None;
            if let Some(raw_format) = &f.format {
                match Format::parse(raw_format) {
                    Some(fmt) if ty == FieldType::String => format = Some(fmt),
                    Some(_) => errors.push(format!(
                        "{at}: format '{raw_format}' applies to a string field, not {}",
                        ty.as_str()
                    )),
                    None => errors.push(format!(
                        "{at}: unknown format '{raw_format}' ({FORMAT_NAMES})"
                    )),
                }
            }
            // A mount field names a slot (§5.1, principle 3), which is what the
            // three refusals below are between them: `access` says how the
            // container opens it and belongs to no other kind of field, and a
            // `default` or an `enum` would be the manifest naming a path on a
            // machine it has never seen.
            let is_mount = format.and_then(Format::mount_kind).is_some();
            let mut access = Access::default();
            if let Some(raw_access) = &f.access {
                match (Access::parse(raw_access), is_mount) {
                    (Some(a), true) => access = a,
                    (Some(_), false) => errors.push(format!(
                        "{at}: access applies to a directory or file field only"
                    )),
                    (None, _) => {
                        errors.push(format!("{at}: access is 'ro' or 'rw', not '{raw_access}'"))
                    }
                }
            }
            if is_mount && f.default.is_some() {
                errors.push(format!(
                    "{at}: a directory or file field cannot have a default — a manifest names a \
                     slot, never a host path"
                ));
            }
            if is_mount && f.enum_values.is_some() {
                errors.push(format!(
                    "{at}: a directory or file field cannot have an enum — a manifest names a \
                     slot, never a host path"
                ));
            }
            let mut enum_values = Vec::new();
            if let Some(values) = &f.enum_values {
                if ty != FieldType::String {
                    errors.push(format!(
                        "{at}: enum applies to a string field, not {}",
                        ty.as_str()
                    ));
                } else if let Some(bad) = values.iter().find(|v| !v.is_string()) {
                    errors.push(format!("{at}: enum value {bad} is not a string"));
                } else {
                    enum_values = values
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect();
                }
            }
            if (f.minimum.is_some() || f.maximum.is_some())
                && !matches!(ty, FieldType::Integer | FieldType::Number)
            {
                errors.push(format!(
                    "{at}: minimum/maximum apply to a number, not {}",
                    ty.as_str()
                ));
            }
            match (&f.items, ty) {
                (Some(items), FieldType::Array) => {
                    match serde_json::from_value::<RawItems>(items.clone()) {
                        Ok(i) if i.ty == "string" => {}
                        Ok(i) => errors.push(format!(
                            "{at}.items.type is '{}'; only an array of string is supported",
                            i.ty
                        )),
                        Err(e) => errors.push(format!("{at}.items: {e}")),
                    }
                }
                (None, FieldType::Array) => errors.push(format!(
                    "{at}: an array field needs items {{\"type\": \"string\"}}"
                )),
                (Some(_), _) => errors.push(format!("{at}: items applies to an array field only")),
                (None, _) => {}
            }
            if let Some(d) = &f.default {
                if let Err(e) = check_type(ty, &enum_values, f.minimum, f.maximum, d) {
                    errors.push(format!("{at}.default: {e}"));
                }
            }
            out.push(Field {
                name: name.clone(),
                ty,
                format,
                title: f.title,
                description: f.description,
                default: f.default,
                enum_values,
                minimum: f.minimum,
                maximum: f.maximum,
                required: self.required.iter().any(|r| r == name),
                access,
            });
        }
        for r in &self.required {
            if !self.properties.keys().any(|k| k == r) {
                errors.push(format!(
                    "config.schema.required names '{r}', which is not a property"
                ));
            }
        }
        if errors.is_empty() {
            Ok(out)
        } else {
            Err(errors)
        }
    }
}

/// One value against one field's declared type, enum and bounds.
fn check_type(
    ty: FieldType,
    enum_values: &[String],
    minimum: Option<f64>,
    maximum: Option<f64>,
    v: &Value,
) -> Result<(), String> {
    let ok = match ty {
        FieldType::String => v.is_string(),
        FieldType::Integer => v.is_i64() || v.is_u64(),
        FieldType::Number => v.is_number(),
        FieldType::Boolean => v.is_boolean(),
        FieldType::Array => v
            .as_array()
            .map(|a| a.iter().all(Value::is_string))
            .unwrap_or(false),
    };
    if !ok {
        return Err(match ty {
            FieldType::Array => format!("expected an array of strings, got {v}"),
            _ => format!("expected {}, got {v}", ty.as_str()),
        });
    }
    if !enum_values.is_empty() {
        if let Some(s) = v.as_str() {
            if !enum_values.iter().any(|e| e == s) {
                return Err(format!("'{s}' is not one of: {}", enum_values.join(", ")));
            }
        }
    }
    if let Some(n) = v.as_f64() {
        if let Some(min) = minimum {
            if n < min {
                return Err(format!("{n} is below the minimum {min}"));
            }
        }
        if let Some(max) = maximum {
            if n > max {
                return Err(format!("{n} is above the maximum {max}"));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Stored config values (§2.6)
// ---------------------------------------------------------------------------

/// The placeholder a secret is returned as: never the value, and distinguishing
/// "configured" from "empty" without leaking either way.
pub fn secret_view(has_value: bool) -> Value {
    serde_json::json!({ "has_value": has_value })
}

/// Validate stored values against the schema — type, required, enum, bounds —
/// with the error naming the field (§2.6).
pub fn validate_values(fields: &[Field], values: &Map<String, Value>) -> Result<(), String> {
    value_errors(fields, values, true)
}

/// The same check, minus "is it complete": every value that **is** there names
/// a field of this agent and fits it, while a required field nobody has filled
/// in yet stays the Run tab's business. What a writer that only supplies some
/// of the form needs to ask — the mail migration ([`super::seed`]) supplies
/// four or five fields and must not be refused because the model is still
/// unpicked.
pub fn validate_present_values(
    fields: &[Field],
    values: &Map<String, Value>,
) -> Result<(), String> {
    value_errors(fields, values, false)
}

fn value_errors(
    fields: &[Field],
    values: &Map<String, Value>,
    complete: bool,
) -> Result<(), String> {
    let mut errors: Vec<String> = Vec::new();
    for key in values.keys() {
        if !fields.iter().any(|f| &f.name == key) {
            errors.push(format!("'{key}' is not a config field of this agent"));
        }
    }
    for f in fields {
        match values.get(&f.name) {
            Some(Value::Null) | None => {
                if complete && f.required && f.default.is_none() {
                    errors.push(format!("'{}' is required", f.name));
                }
            }
            Some(v) => {
                if let Err(e) = check_type(f.ty, &f.enum_values, f.minimum, f.maximum, v) {
                    errors.push(format!("'{}': {e}", f.name));
                } else if complete && f.required && v.as_str().map(str::is_empty).unwrap_or(false) {
                    errors.push(format!("'{}' is required", f.name));
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Stored values over schema defaults — the `config` root a template resolves
/// against (§2.3).
pub fn effective_values(fields: &[Field], values: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for f in fields {
        let v = match values.get(&f.name) {
            Some(Value::Null) | None => f.default.clone(),
            Some(v) => Some(v.clone()),
        };
        if let Some(v) = v {
            out.insert(f.name.clone(), v);
        }
    }
    Value::Object(out)
}

/// Stored values with every `secret` field replaced by [`secret_view`]. What
/// the API returns, always.
pub fn masked_values(fields: &[Field], values: &Map<String, Value>) -> Value {
    let mut out = values.clone();
    for f in fields.iter().filter(|f| f.is_secret()) {
        let present = values
            .get(&f.name)
            .map(|v| !v.is_null() && v.as_str() != Some(""))
            .unwrap_or(false);
        out.insert(f.name.clone(), secret_view(present));
    }
    out.retain(|k, _| fields.iter().any(|f| &f.name == k));
    Value::Object(out)
}

/// Stored values with every `secret` field **removed**. Used by export and by
/// `agent_duplicate`: a secret leaves this process in neither.
///
/// A key the schema no longer declares goes too, the same way [`masked_values`]
/// drops it from a read. Replacing a manifest keeps the stored config (§5), so
/// a field that *used* to be a secret leaves its value behind in the column
/// with nothing left to mark it as one — keeping such a key would write that
/// value into an export in the clear.
pub fn without_secrets(fields: &[Field], values: &Map<String, Value>) -> Map<String, Value> {
    let mut out = values.clone();
    out.retain(|k, _| fields.iter().any(|f| &f.name == k && !f.is_secret()));
    out
}

/// The names of the secret fields that were dropped — the export's
/// `config_omitted` list, so the receiver knows what to fill in (§5).
pub fn secret_names(fields: &[Field]) -> Vec<String> {
    fields
        .iter()
        .filter(|f| f.is_secret())
        .map(|f| f.name.clone())
        .collect()
}

/// [`without_secrets`], and **without the mount values too** — what an export
/// writes (mounts §5.2).
///
/// A secret leaves this process because it is a credential; a mount value
/// leaves it because it is a path on *this* box, and principle 3 is that a
/// manifest names a slot and only the owner says which folder fills it. An
/// export that carried `/home/alice/Notes` would be asking the receiver's
/// gateway to bind a directory it has never seen.
pub fn without_secrets_or_mounts(
    fields: &[Field],
    values: &Map<String, Value>,
) -> Map<String, Value> {
    let mut out = without_secrets(fields, values);
    out.retain(|k, _| !fields.iter().any(|f| &f.name == k && f.is_mount()));
    out
}

/// The names of the mount fields — the export's `config_unbound` list, so the
/// receiver knows there is a slot to bind (§5.2).
///
/// Every declared mount field, not only the ones that had a value: an unbound
/// slot is exactly what the receiver has to fill in, and a list that named only
/// the exporter's bound ones would go quiet about the rest.
pub fn mount_names(fields: &[Field]) -> Vec<String> {
    fields
        .iter()
        .filter(|f| f.is_mount())
        .map(|f| f.name.clone())
        .collect()
}

/// Merge a submitted config over the stored one.
///
/// Sparse, like every other patch in this codebase: a key the submission does
/// not mention keeps its stored value. A `secret` field submitted empty (or as
/// the `{ "has_value": … }` view it was read back as) keeps the stored secret —
/// the house convention for tokens (§2.6).
pub fn merge_values(
    fields: &[Field],
    stored: &Map<String, Value>,
    incoming: &Map<String, Value>,
) -> Map<String, Value> {
    let mut out = stored.clone();
    for (k, v) in incoming {
        let secret = fields.iter().any(|f| &f.name == k && f.is_secret());
        if secret {
            let keeps = v.is_null() || v.as_str() == Some("") || v.is_object();
            if keeps {
                continue;
            }
        }
        out.insert(k.clone(), v.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// Parsing and validation
// ---------------------------------------------------------------------------

/// Roots resolvable before any item exists: the config form, the agent's own
/// identity, the run id.
const STATIC_ROOTS: &[Root] = &[Root::Config, Root::Agent, Root::Run];
/// Plus the source item, before its fetch has run.
const SOURCE_ITEM_ROOTS: &[Root] = &[Root::Config, Root::Item, Root::Agent, Root::Run];
/// Plus the fetch result.
const ITEM_ROOTS: &[Root] = &[
    Root::Config,
    Root::Item,
    Root::Fetched,
    Root::Agent,
    Root::Run,
];
/// The apply step sees the reviewed rows instead of a single item.
const APPLY_ROOTS: &[Root] = &[Root::Config, Root::Rows, Root::Agent, Root::Run];

/// `[a-z0-9][a-z0-9-]{0,63}` (§2.1), minus the ids the route table owns.
pub fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("id is required".to_string());
    }
    if id.len() > 64 {
        return Err(format!(
            "id '{id}' is {} characters; the limit is 64",
            id.len()
        ));
    }
    let mut chars = id.chars();
    let first = chars.next().unwrap_or(' ');
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(format!(
            "id '{id}' must start with a lowercase letter or a digit"
        ));
    }
    if let Some(bad) = chars.find(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && *c != '-') {
        return Err(format!(
            "id '{id}' contains '{bad}'; allowed: lowercase letters, digits and '-'"
        ));
    }
    if RESERVED_IDS.contains(&id) {
        return Err(format!(
            "id '{id}' is reserved by the /api/agents routes; pick another"
        ));
    }
    Ok(())
}

/// Parse and validate a manifest document.
///
/// `schema_version` is read off the raw JSON **before** deserialization, so an
/// unknown version is reported as such instead of as a pile of unknown-field
/// errors from a document this build was never meant to read (§2.1).
///
/// The version peek parses into a [`Value`] and the real deserialization then
/// runs against the **text** again rather than against that value. Two passes
/// over a few kilobytes, in exchange for two things a `from_value` pass cannot
/// give: serde's line and column in every error, and duplicate-key detection —
/// `serde_json::Value` is a `BTreeMap`, so it would have already resolved a
/// duplicate last-wins and reordered the config form's fields alphabetically.
pub fn load(json: &str) -> Result<Manifest, String> {
    let raw: Value =
        serde_json::from_str(json).map_err(|e| format!("the manifest is not valid JSON: {e}"))?;
    if !raw.is_object() {
        return Err("the manifest must be a JSON object".to_string());
    }
    match raw.get("schema_version") {
        None => {
            return Err(format!(
                "the manifest has no 'schema_version'; this build understands version \
                 {SCHEMA_VERSION}"
            ))
        }
        Some(v) if v.as_u64() == Some(SCHEMA_VERSION) => {}
        Some(v) => {
            return Err(format!(
                "manifest schema_version {v} is not supported; this build understands version \
                 {SCHEMA_VERSION}"
            ))
        }
    }
    let m: Manifest = serde_json::from_str(json).map_err(|e| format!("manifest: {e}"))?;
    m.validate()?;
    Ok(m)
}

impl Manifest {
    /// The bounds this run declares, or the printed defaults when it declares
    /// none (container-runtime §4.1). A `chat` agent has no run of its own to
    /// bound and takes the defaults too — nothing reads them there.
    pub fn limits(&self) -> Limits {
        match &self.run {
            RunSpec::Batch {
                limits: Some(l), ..
            }
            | RunSpec::Container {
                limits: Some(l), ..
            } => l.clone(),
            _ => Limits::default(),
        }
    }

    /// The phases this manifest's run kind implements, in declaration order.
    ///
    /// A container says so itself (`run.phases`, default `["run"]`); a batch
    /// manifest's phases are the pipeline's, derived from what it declares.
    pub fn phases(&self) -> Vec<String> {
        match &self.run {
            RunSpec::Container { phases, .. } => phases.clone(),
            RunSpec::Batch { item, apply, .. } => {
                let mut out = vec!["list".to_string()];
                if item.user.is_some() {
                    out.push("classify".to_string());
                    out.push("rerun".to_string());
                }
                if apply.is_some() {
                    out.push("apply".to_string());
                }
                out
            }
            RunSpec::Chat { .. } => Vec::new(),
        }
    }

    /// The schema one phase's `output` event is validated against at close
    /// (§3.2). `None` means that phase is unvalidated, which is a legal and
    /// visible choice, not an oversight. Only a container run spec carries any.
    pub fn output_schema(&self, phase: &str) -> Option<&Value> {
        match &self.run {
            RunSpec::Container { output, .. } => output.as_ref()?.get(phase),
            // A `script` apply step carries its own (container-runtime §4.2),
            // and it is checked by the same `ledger::check_output` at close —
            // one rule for both, so a script that returns the wrong shape fails
            // the job instead of storing it.
            RunSpec::Batch {
                apply: Some(step), ..
            } if phase == "apply" => step.output.as_ref(),
            _ => None,
        }
    }

    /// The batch pipeline's apply step, when it declares one.
    pub fn apply_step(&self) -> Option<&Step> {
        match &self.run {
            RunSpec::Batch { apply, .. } => apply.as_ref(),
            _ => None,
        }
    }

    /// The phases this manifest validates the `output` of, in declaration
    /// order — what the Runtime block prints as "validated" against the rest.
    pub fn output_validated(&self) -> Vec<String> {
        match &self.run {
            RunSpec::Container {
                output: Some(map), ..
            } => map.keys().cloned().collect(),
            _ => Vec::new(),
        }
    }

    /// The image a container agent runs, when it declares one.
    pub fn image(&self) -> Option<&str> {
        match &self.run {
            RunSpec::Container { image, .. } => image.as_deref(),
            _ => None,
        }
    }

    /// `run.pull` — the visible policy, never implicit (§4.1). Every other run
    /// kind answers with the same `never` default, because the one image they
    /// can reach (the script image, from Settings) is not the manifest's to
    /// choose a policy for.
    pub fn pull(&self) -> PullPolicy {
        match &self.run {
            RunSpec::Container { pull, .. } => *pull,
            _ => PullPolicy::default(),
        }
    }

    /// Every problem with this manifest, in document order. Empty ⇒ valid.
    pub fn errors(&self) -> Vec<String> {
        let mut errors: Vec<String> = Vec::new();
        if self.schema_version != SCHEMA_VERSION {
            errors.push(format!(
                "schema_version {} is not supported; this build understands version \
                 {SCHEMA_VERSION}",
                self.schema_version
            ));
        }
        if let Err(e) = validate_id(&self.id) {
            errors.push(e);
        }
        if self.name.trim().is_empty() {
            errors.push("name is required".to_string());
        }

        // The config schema first: every other check needs its field names.
        let fields = match self.config.as_ref() {
            None => Vec::new(),
            Some(block) => match block.schema.fields() {
                Ok(f) => f,
                Err(mut e) => {
                    errors.append(&mut e);
                    Vec::new()
                }
            },
        };
        let names: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();

        if self.model.alias.trim().is_empty() {
            errors.push("model.alias is required".to_string());
        }
        template::validate(
            &self.model.alias,
            "model.alias",
            STATIC_ROOTS,
            &names,
            &mut errors,
        );

        // An agent that registers its own tools (`run.provides.mcp`) publishes
        // them under the prefix that *is* its id (container-runtime §3.3), so a
        // name in its own allow list is the agent asking the gateway for a tool
        // the gateway reaches by calling the agent.
        let own_prefix = matches!(
            &self.run,
            RunSpec::Container { provides: Some(p), .. } if p.mcp.is_some()
        )
        .then(|| format!("{}__", self.id));
        for (i, t) in self.tools.iter().enumerate() {
            if t.label.trim().is_empty() {
                errors.push(format!("tools[{i}].label is required"));
            }
            if let Some(allowed) = &t.allowed {
                for (j, name) in allowed.iter().enumerate() {
                    if name.trim().is_empty() {
                        errors.push(format!("tools[{i}].allowed[{j}] is empty"));
                    }
                    if own_prefix.as_deref().is_some_and(|p| name.starts_with(p)) {
                        errors.push(format!(
                            "tools[{i}].allowed[{j}] is '{name}', one of this agent's own tools: \
                             run.provides.mcp registers them under the prefix '{}', so calling \
                             one would go out through /mcp, back in through this agent's app \
                             proxy and into the container that made the call. An agent reaches \
                             its own code directly, not through the gateway — remove it from the \
                             allow list.",
                            self.id
                        ));
                    }
                }
            }
            if let Some(install) = &t.install {
                if install.reference.trim().is_empty() {
                    errors.push(format!("tools[{i}].install.ref is required"));
                }
            }
        }

        match &self.run {
            RunSpec::Chat { system } => {
                if let Some(s) = system {
                    template::validate(s, "run.system", STATIC_ROOTS, &names, &mut errors);
                    reject_secrets_in_prompt(s, "run.system", &fields, &mut errors);
                }
            }
            RunSpec::Container {
                image,
                pull: _,
                entrypoint,
                args,
                columns,
                review,
                phases,
                // Bounds, not behaviour: `Limits`' own deserializer has already
                // refused a negative one naming the field.
                limits: _,
                service,
                provides,
                output,
            } => {
                self.container_errors(
                    image,
                    entrypoint,
                    args,
                    columns,
                    review,
                    phases,
                    service,
                    provides,
                    output,
                    &mut errors,
                );
            }
            RunSpec::Batch {
                source,
                items_path,
                item,
                review,
                apply,
                // Bounds, not behaviour: `Limits`' own deserializer has
                // already refused a negative one naming the field.
                limits: _,
            } => {
                validate_step(
                    source,
                    "run.source",
                    STATIC_ROOTS,
                    &names,
                    &fields,
                    &mut errors,
                );
                if let Some(p) = items_path {
                    if !p.is_empty() && !p.starts_with('/') {
                        errors.push(format!(
                            "run.items_path '{p}' is not a JSON pointer; it must start with '/'"
                        ));
                    }
                }
                if item.id.trim().is_empty() {
                    errors.push("run.item.id is required — it is the row identity".to_string());
                }
                template::validate(
                    &item.id,
                    "run.item.id",
                    SOURCE_ITEM_ROOTS,
                    &names,
                    &mut errors,
                );
                if let Some(fetch) = &item.fetch {
                    validate_step(
                        fetch,
                        "run.item.fetch",
                        SOURCE_ITEM_ROOTS,
                        &names,
                        &fields,
                        &mut errors,
                    );
                }
                for (col, tmpl) in item.columns.iter() {
                    template::validate(
                        tmpl,
                        &format!("run.item.columns.{col}"),
                        ITEM_ROOTS,
                        &names,
                        &mut errors,
                    );
                }
                if let Some(s) = &item.system {
                    template::validate(s, "run.item.system", ITEM_ROOTS, &names, &mut errors);
                    reject_secrets_in_prompt(s, "run.item.system", &fields, &mut errors);
                }
                if let Some(u) = &item.user {
                    template::validate(u, "run.item.user", ITEM_ROOTS, &names, &mut errors);
                    reject_secrets_in_prompt(u, "run.item.user", &fields, &mut errors);
                }
                if let Some(c) = &item.concurrency {
                    match c {
                        Value::String(s) => template::validate(
                            s,
                            "run.item.concurrency",
                            STATIC_ROOTS,
                            &names,
                            &mut errors,
                        ),
                        Value::Number(n) if n.is_u64() || n.is_i64() => {}
                        other => errors.push(format!(
                            "run.item.concurrency must be an integer or a template, got {other}"
                        )),
                    }
                }
                // A classify stage needs both halves; neither is the list-only
                // agent, which is a legitimate shape (§2.4).
                match (&item.user, &item.output) {
                    (Some(_), None) => errors.push(
                        "run.item.user is set but run.item.output is not — a classify \
                               call needs the schema it answers with"
                            .to_string(),
                    ),
                    (None, Some(_)) => errors.push(
                        "run.item.output is set but run.item.user is not — there is no prompt \
                         to classify with"
                            .to_string(),
                    ),
                    _ => {}
                }
                let output_fields = match &item.output {
                    None => Vec::new(),
                    Some(o) => validate_item_output(o, &names, &fields, &mut errors),
                };
                if let Some(r) = review {
                    for f in &r.editable {
                        if !output_fields.iter().any(|o| o == f) {
                            errors.push(format!(
                                "run.review.editable names '{f}', which the item output does \
                                 not produce{}",
                                if output_fields.is_empty() {
                                    String::new()
                                } else {
                                    format!(" (fields: {})", output_fields.join(", "))
                                }
                            ));
                        }
                    }
                }
                if let Some(apply) = apply {
                    validate_step(
                        apply,
                        "run.apply",
                        APPLY_ROOTS,
                        &names,
                        &fields,
                        &mut errors,
                    );
                }
            }
        }
        errors
    }

    /// `run.kind = "container"` (container-runtime §4.1, §4.3), split out
    /// because [`Self::errors`]' match arm would otherwise be a page long.
    #[allow(clippy::too_many_arguments)]
    fn container_errors(
        &self,
        image: &Option<String>,
        entrypoint: &Option<String>,
        args: &[String],
        columns: &[String],
        review: &Option<Review>,
        phases: &[String],
        service: &Option<Service>,
        provides: &Option<Provides>,
        output: &Option<OrderedMap<Value>>,
        errors: &mut Vec<String>,
    ) {
        match image {
            None if service.is_none() => errors.push(
                "run.image is required for a container agent: the run and apply phases start an \
                 image, and only a manifest that declares a `service` (proxied from a dev_url) \
                 may leave it out"
                    .to_string(),
            ),
            Some(i) if i.trim().is_empty() => errors.push("run.image is empty".to_string()),
            _ => {}
        }
        if let Some(e) = entrypoint {
            if e.trim().is_empty() {
                errors.push(
                    "run.entrypoint is empty; leave it out to use the image's own".to_string(),
                );
            }
        }
        for (i, a) in args.iter().enumerate() {
            if a.is_empty() {
                errors.push(format!("run.args[{i}] is empty"));
            }
        }
        for (i, c) in columns.iter().enumerate() {
            if c.trim().is_empty() {
                errors.push(format!("run.columns[{i}] is empty"));
            } else if columns[..i].iter().any(|prev| prev == c) {
                errors.push(format!("run.columns names '{c}' twice"));
            }
        }
        if phases.is_empty() {
            errors.push(
                "run.phases is empty; a container implements at least 'run' (leave the field out \
                 for that default)"
                    .to_string(),
            );
        }
        for (i, p) in phases.iter().enumerate() {
            if !matches!(p.as_str(), "run" | "apply") {
                errors.push(format!(
                    "run.phases[{i}] is '{p}'; a container implements 'run' and optionally 'apply'"
                ));
            } else if phases[..i].iter().any(|prev| prev == p) {
                errors.push(format!("run.phases names '{p}' twice"));
            }
        }
        if let Some(r) = review {
            if !phases.iter().any(|p| p == "apply") {
                errors.push(
                    "run.review is set but run.phases has no 'apply' — the review table is the \
                     gate in front of a phase this image does not implement"
                        .to_string(),
                );
            }
            for (i, f) in r.editable.iter().enumerate() {
                if f.trim().is_empty() {
                    errors.push(format!("run.review.editable[{i}] is empty"));
                }
            }
        }
        if let Some(s) = service {
            if s.port == 0 || s.port > 65535 {
                errors.push(format!(
                    "run.service.port is {}; a TCP port is 1–65535",
                    s.port
                ));
            }
            // Blank is a *choice*, not an omission: it selects a TCP connect
            // instead of an HTTP GET, for an image whose port speaks something
            // an HTTP request cannot introduce itself to (§3.3). Anything else
            // has to be a path.
            if !s.health_path.is_empty() && !s.health_path.starts_with('/') {
                errors.push(format!(
                    "run.service.health_path '{}' must start with '/', or be empty to health-\
                     check with a TCP connect to run.service.port instead",
                    s.health_path
                ));
            }
        }
        if let Some(p) = provides {
            if let Some(path) = &p.mcp {
                if service.is_none() {
                    errors.push(
                        "run.provides.mcp needs run.service: the MCP route proxies the same \
                         container the App tab does"
                            .to_string(),
                    );
                }
                if !path.starts_with('/') {
                    errors.push(format!("run.provides.mcp '{path}' must start with '/'"));
                }
                // The registration's `tool_prefix` is the agent id, so an id in
                // the reserved namespace would shadow a built-in toolset.
                if let Some((ns, what)) = crate::mcp::RESERVED_NAMESPACES
                    .iter()
                    .find(|(ns, _)| *ns == self.id)
                {
                    errors.push(format!(
                        "run.provides.mcp would register this agent's tools under the prefix \
                         '{ns}', which is reserved for {what}; rename the agent"
                    ));
                }
            }
        }
        if let Some(map) = output {
            if map.is_empty() {
                errors.push(
                    "run.output is empty; leave it out for a container whose phases report no \
                     structured output"
                        .to_string(),
                );
            }
            for (phase, schema) in map.iter() {
                if !phases.iter().any(|p| p == phase) {
                    errors.push(format!(
                        "run.output names the phase '{phase}', which run.phases does not declare \
                         ({})",
                        phases.join(", ")
                    ));
                }
                check_output_schema(schema, &format!("run.output.{phase}"), errors);
            }
        }
    }

    /// [`Self::errors`] joined into one message, the shape the `/api` plane
    /// reports failures in.
    pub fn validate(&self) -> Result<(), String> {
        let errors = self.errors();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// The config fields, or the reasons there are none.
    pub fn fields(&self) -> Result<Vec<Field>, Vec<String>> {
        match &self.config {
            None => Ok(Vec::new()),
            Some(block) => block.schema.fields(),
        }
    }

    /// The mount slots this manifest declares, in the form's order (§5.1).
    ///
    /// The query everything that mounts, checks or prints a mount iterates: the
    /// argv and `input.json` (§5.5, §5.6), the store-time path rules (§5.3) and
    /// the Definition tab's *Mounts* heading all ask the manifest this one
    /// question rather than each re-deriving "which format counts".
    ///
    /// A manifest whose config schema does not parse has **no** mount fields
    /// here — the same reading [`Self::fields`]' callers already take, and its
    /// errors are reported where the document is loaded, not from a query.
    pub fn mount_fields(&self) -> impl Iterator<Item = MountField> {
        self.fields()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|f| f.mount())
    }

    /// Whether anything on this manifest has to be mounted — what decides
    /// `--userns=keep-id`, bound or not (§5.5).
    pub fn declares_mounts(&self) -> bool {
        self.mount_fields().next().is_some()
    }

    /// Canonical serialization — what is stored and what is exported, so a
    /// round trip is byte-stable (§8) and a manifest cannot smuggle in fields
    /// this build refused to read.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn kind(&self) -> &'static str {
        self.run.kind()
    }

    /// Every tool label this agent attaches, in manifest order.
    pub fn labels(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.label.clone()).collect()
    }
}

/// Refuse a `secret` config field in a **model prompt** (§2.6).
///
/// A prompt is not just model-visible, it is persisted in the clear on the way
/// past: a `chat` agent's rendered `system` becomes `chat_threads.
/// system_prompt`, which the Chat page reads back and edits, and a classify
/// call's rendered `user` lands in the request log. Neither is a place a token
/// can be taken out of again, so the manifest is refused before the value ever
/// exists.
///
/// A **tool argument** is the opposite case and stays allowed: handing
/// `{{config.api_token}}` to an MCP call is what a secret config field is for.
/// The rule is about where the text goes, not about the placeholder.
fn reject_secrets_in_prompt(template: &str, at: &str, fields: &[Field], errors: &mut Vec<String>) {
    for p in template::placeholders(template) {
        let mut parts = p.path.split('.');
        if parts.next() != Some("config") {
            continue;
        }
        let Some(name) = parts.next() else { continue };
        if fields.iter().any(|f| f.name == name && f.is_secret()) {
            errors.push(format!(
                "{at}: '{{{{{}}}}}' puts the secret field '{name}' into a model prompt, which is \
                 stored in the clear (a thread keeps its system prompt, a run logs its request). \
                 A secret belongs in a tool argument, not in a prompt.",
                p.path
            ));
        }
    }
}

fn validate_step(
    step: &Step,
    at: &str,
    roots: &[Root],
    names: &[String],
    fields: &[Field],
    errors: &mut Vec<String>,
) {
    // Exactly one of the three shapes (container-runtime §4.3). Reported first
    // and by name, because "a step is either … or …" is the one error an author
    // can act on without reading the schema.
    let present: Vec<&str> = [
        step.tool.as_ref().map(|_| "tool"),
        step.turn.as_ref().map(|_| "turn"),
        step.script.as_ref().map(|_| "script"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let ambiguous = present.len() > 1;
    if ambiguous {
        errors.push(format!(
            "{at}: a step is exactly one of 'tool' (a direct call), 'turn' or 'script'; this one \
             declares {}",
            present.join(" and ")
        ));
    }
    // `output` is the script's schema; a turn carries its own `turn.output` and
    // a direct call returns the tool's result unchanged. Reported even when the
    // step is ambiguous: an author fixing one error should not have to save
    // again to be told about the next one.
    if step.output.is_some() && step.script.is_none() {
        errors.push(format!(
            "{at}.output belongs to a script step — a turn declares its own 'turn.output', and a \
             direct call returns the tool's result"
        ));
    }
    if ambiguous {
        // Which shape's rules to apply below is exactly what is undecided.
        return;
    }
    if let Some(script) = &step.script {
        if script.is_empty() {
            errors.push(format!(
                "{at}.script is empty; it is an ES module exporting the phase's function"
            ));
        }
        if step.args.is_some() {
            errors.push(format!(
                "{at}.args belongs to a direct call, not to a script — a script reads ctx.config \
                 and ctx.rows"
            ));
        }
        if let Some(schema) = &step.output {
            check_output_schema(schema, &format!("{at}.output"), errors);
        }
        return;
    }
    match (&step.tool, &step.turn) {
        (Some(tool), None) => {
            if tool.trim().is_empty() {
                errors.push(format!("{at}.tool is empty"));
            }
            match &step.args {
                None => {}
                Some(Value::Object(_)) => {
                    template::validate_value(
                        step.args.as_ref().unwrap(),
                        &format!("{at}.args"),
                        roots,
                        names,
                        errors,
                    );
                }
                Some(other) => errors.push(format!("{at}.args must be an object, got {other}")),
            }
        }
        (None, Some(turn)) => {
            if step.args.is_some() {
                errors.push(format!("{at}.args belongs to a direct call, not to a turn"));
            }
            if turn.prompt.trim().is_empty() {
                errors.push(format!("{at}.turn.prompt is required"));
            }
            template::validate(
                &turn.prompt,
                &format!("{at}.turn.prompt"),
                roots,
                names,
                errors,
            );
            // A turn's prompt *is* a model prompt — the executor renders both
            // of these straight into `Message`s — so the §2.6 rule that keeps a
            // secret out of `run.item.user` has to hold here too.
            reject_secrets_in_prompt(&turn.prompt, &format!("{at}.turn.prompt"), fields, errors);
            if let Some(s) = &turn.system {
                template::validate(s, &format!("{at}.turn.system"), roots, names, errors);
                reject_secrets_in_prompt(s, &format!("{at}.turn.system"), fields, errors);
            }
            for (i, t) in turn.tools.iter().enumerate() {
                if t.trim().is_empty() {
                    errors.push(format!("{at}.turn.tools[{i}] is empty"));
                }
            }
            if let Some(schema) = &turn.output {
                check_output_schema(schema, &format!("{at}.turn.output"), errors);
            }
        }
        // Both are impossible here: the arity check above returned already.
        (Some(_), Some(_)) => {}
        (None, None) => errors.push(format!(
            "{at}: a step needs one of 'tool' (a direct call), 'turn' or 'script'"
        )),
    }
}

/// Structural check on a schema handed to the model as `response_format`. It is
/// not rendered as a form, so the subset does not apply — but a schema that is
/// not an object schema will fail at the upstream with a far worse message.
fn check_output_schema(schema: &Value, at: &str, errors: &mut Vec<String>) {
    let Some(obj) = schema.as_object() else {
        errors.push(format!("{at} must be a JSON object schema"));
        return;
    };
    match obj.get("type").and_then(Value::as_str) {
        Some("object") => {}
        Some(other) => errors.push(format!(
            "{at}.type is '{other}'; a structured output is an object schema"
        )),
        None => errors.push(format!("{at} has no 'type'; expected \"object\"")),
    }
    match obj.get("properties") {
        None => errors.push(format!("{at} has no 'properties'")),
        Some(Value::Object(p)) if p.is_empty() => errors.push(format!("{at}.properties is empty")),
        Some(Value::Object(_)) => {}
        Some(other) => errors.push(format!("{at}.properties must be an object, got {other}")),
    }
    if let Some(req) = obj.get("required") {
        match req.as_array() {
            Some(a) if a.iter().all(Value::is_string) => {}
            _ => errors.push(format!("{at}.required must be an array of field names")),
        }
    }
}

/// Returns the output field names, which `review.editable` is checked against.
fn validate_item_output(
    o: &ItemOutput,
    names: &[String],
    fields: &[Field],
    errors: &mut Vec<String>,
) -> Vec<String> {
    let at = "run.item.output";
    match (&o.enum_from, &o.schema) {
        (Some(path), None) => {
            let Some(field) = &o.field else {
                errors.push(format!("{at}.field is required with enum_from"));
                return Vec::new();
            };
            if field.trim().is_empty() {
                errors.push(format!("{at}.field is empty"));
            }
            let Some(config_field) = path.strip_prefix("config.") else {
                errors.push(format!(
                    "{at}.enum_from is '{path}'; it must name a config field as \
                     config.<field>"
                ));
                return vec![field.clone()];
            };
            match fields.iter().find(|f| f.name == config_field) {
                None => errors.push(format!(
                    "{at}.enum_from names no config field ({})",
                    if names.is_empty() {
                        "this agent declares no config schema".to_string()
                    } else {
                        format!("fields: {}", names.join(", "))
                    }
                )),
                Some(f) if f.ty != FieldType::Array => errors.push(format!(
                    "{at}.enum_from names '{config_field}', which is {} — an enum is built \
                     from an array of string",
                    f.ty.as_str()
                )),
                Some(_) => {}
            }
            match &o.fallback {
                Some(Value::String(s)) if !s.is_empty() => {}
                Some(other) => errors.push(format!(
                    "{at}.fallback must be a non-empty string with enum_from, got {other}"
                )),
                None => errors.push(format!(
                    "{at}.fallback is required with enum_from — it is what a failed or \
                     unconvinced call answers"
                )),
            }
            vec![field.clone()]
        }
        (None, Some(schema)) => {
            if o.field.is_some() {
                errors.push(format!("{at}.field belongs to the enum_from form"));
            }
            check_output_schema(schema, at, errors);
            schema
                .get("properties")
                .and_then(Value::as_object)
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default()
        }
        (Some(_), Some(_)) => {
            errors.push(format!(
                "{at}: use either enum_from (a single classified field) or schema, not both"
            ));
            Vec::new()
        }
        (None, None) => {
            errors.push(format!("{at}: needs either enum_from or schema"));
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests;
