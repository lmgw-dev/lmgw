//! Built-in self-admin tools (§20) — lmgw administering itself over its own
//! northbound `/mcp`.
//!
//! The gateway is already an MCP server, so the cheapest way to let an agent
//! drive it is to add tools to the catalog it already serves: no new transport,
//! no new auth, and every call lands in `request_logs` like any other. This
//! module owns the tool *surface* — names, schemas, mode gating, result shape —
//! and nothing else; every action behind it is a call into [`crate::ops`],
//! which the dashboard's HTML handlers share.
//!
//! **Schema shape.** Every argument is a flat scalar: string, integer, boolean,
//! or a string `enum`. No nested objects, no arrays. A tool whose parameters
//! need hand-built JSON is a tool a small local model cannot call reliably, and
//! these are meant to be callable by whatever is pointed at the gateway. Where
//! the underlying record genuinely holds a list (an MCP server's `args`, `env`,
//! `headers`), the argument is newline-delimited text in the same syntax the
//! dashboard's textareas accept.
//!
//! **Gating.** [`SelfAdmin`] decides what is listed and what is callable;
//! `read_only` (the default) exposes the read tools and refuses the rest with
//! an error that names the setting to change. See [`crate::ops::check_mode`].
//! `writes` is about *effect*, not about touching the database:
//! `lmgw__local_model_test` changes no configuration but loads a model off
//! disk and pins VRAM, which is not something a read-only grant should do.
//!
//! **Descriptions are the documentation.** The intended caller has no access
//! to this source and no shell on the box, so anything it must know to
//! configure a model correctly — that a projector needs `--mmproj`, that
//! `spec_type` has to match the drafter's architecture, that `apply` is
//! required and does not prove anything loads — has to be in a description,
//! a returned payload, or an error message. Tool results therefore carry
//! `message`/`hint`/`next_step` fields naming the tool to call next.

use serde_json::{json, Map, Value};

use crate::config::SelfAdmin;
use crate::ops;
use crate::state::SharedState;

use super::CallError;

/// `lmgw__candidate_alias_set`'s schema and description (candidate-aliases
/// design §4.1, §6) — a child module for the same file-size reason
/// [`crate::ops::candidate_alias`] is a child of [`crate::ops`]; this file
/// keeps only the one dispatch arm in [`run`] — the line that adds it to
/// [`catalog`] lives in `catalog/routing.rs`.
mod candidate_alias;

/// `lmgw__bench_*`'s dispatch (benchmark design §8.1) — a child module for
/// the same file-size reason; the tools themselves are `catalog/bench.rs`.
mod bench;

/// `lmgw__audio_catalog` / `lmgw__audio_model_set` dispatch (realtime fix
/// package B7) — the tools themselves are `catalog/audio.rs`.
mod audio;

/// The access settings no tool changes (client-apps design L5's notes,
/// 2026-10-07).
mod guards;
pub use guards::ACCESS_SETTINGS;

/// Prefix every tool in this module carries — the namespace reserved from
/// southbound servers by [`super::RESERVED_TOOL_NAMESPACE`].
pub const PREFIX: &str = super::RESERVED_TOOL_NAMESPACE;

/// Whether an exposed tool name belongs to this plane rather than to an
/// aggregated southbound server. Checked *before* the mode gate, so a call made
/// while self-admin is off gets the explanatory refusal rather than a confusing
/// "unknown tool".
pub fn owns(name: &str) -> bool {
    name.starts_with(PREFIX)
}

// ---------------------------------------------------------------------------
// Schema helpers — keep the tool definitions in `catalog/` readable
// ---------------------------------------------------------------------------

fn str_p(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}
fn int_p(desc: &str) -> Value {
    json!({ "type": "integer", "description": desc })
}
fn bool_p(desc: &str) -> Value {
    json!({ "type": "boolean", "description": desc })
}
/// A fractional number. Distinct from [`int_p`] because a sampler value like
/// `top_p: 0.95` rejected as "must be an integer" is a confusing dead end.
fn num_p(desc: &str) -> Value {
    json!({ "type": "number", "description": desc })
}
fn enum_p(desc: &str, values: &[&str]) -> Value {
    json!({ "type": "string", "enum": values, "description": desc })
}
/// A ladder's higher rungs (ladder design §4.1, §6): a JSON array of
/// `{gguf_path, ctx_size}` objects, given as a JSON-encoded **string** —
/// every argument in this module stays a flat scalar (module doc: "no
/// nested objects, no arrays"; pinned by `all_parameters_are_flat_scalars`),
/// the same "structured value, flat schema" shape `capabilities_override`
/// already uses. `hoist_json_arg` parses it, before `patch_from_args`'s
/// generic deserialize, into the real `Vec<Rung>`
/// `LocalModelPatch.ladder` expects — unlike `capabilities_override`
/// (`Option<Value>`, parsed downstream at save time), `ladder`'s patch field
/// is already the typed array, so this one has to unwrap the string a step
/// earlier, at the MCP layer, or `serde_json::from_value` would see a string
/// where an array belongs and refuse the whole call.
fn ladder_p(desc: &str) -> Value {
    str_p(desc)
}

/// One built-in tool: its JSON-RPC entry plus whether it mutates config.
struct Builtin {
    name: &'static str,
    /// `true` ⇒ requires [`SelfAdmin::Full`]; `false` ⇒ [`SelfAdmin::ReadOnly`].
    writes: bool,
    description: &'static str,
    props: Vec<(&'static str, Value)>,
    required: &'static [&'static str],
}

impl Builtin {
    /// The `tools/list` entry. Schemas are closed (`additionalProperties:
    /// false`) so a client that hallucinates an argument gets told, rather than
    /// having it silently dropped.
    fn to_entry(&self) -> Value {
        let mut properties = Map::new();
        for (k, v) in &self.props {
            properties.insert((*k).to_string(), v.clone());
        }
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": {
                "type": "object",
                "properties": Value::Object(properties),
                "required": self.required,
                "additionalProperties": false,
            },
        })
    }
}

mod catalog;

use catalog::catalog;

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// The built-in entries visible at `mode`, as `tools/list` array elements.
/// `Off` yields nothing; `ReadOnly` yields the read tools; `Full` yields all.
pub fn list(mode: SelfAdmin) -> Vec<Value> {
    if !mode.allows_read() {
        return Vec::new();
    }
    catalog()
        .iter()
        .filter(|t| !t.writes || mode.allows_write())
        .map(Builtin::to_entry)
        .collect()
}

/// The whole catalog with the mode gate **not** applied: `(entry, writes)`.
///
/// The tool inventory needs every tool this plane has plus what the gate would
/// do to it — [`list`] alone cannot distinguish "there is no such tool" from
/// "it is hidden because self-admin is read-only", and the inventory's whole
/// job is to say which.
pub fn full_catalog() -> Vec<(Value, bool)> {
    catalog().iter().map(|t| (t.to_entry(), t.writes)).collect()
}

// ---------------------------------------------------------------------------
// Result shaping
// ---------------------------------------------------------------------------

/// A successful tool result: the JSON payload pretty-printed into a single text
/// block. Text rather than `structuredContent` because a plain content block is
/// what every client — and every small model — reads without an output schema.
fn ok_result(v: &Value) -> Value {
    let text = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

/// A failed tool result. Modelled as `isError: true` rather than a JSON-RPC
/// error because these are *tool* failures the caller can fix and retry (bad
/// argument, missing row, refused by the mode gate) — the model needs to read
/// the message. Genuine protocol errors (an unknown tool name) stay JSON-RPC.
pub fn err_result(msg: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Read one optional scalar argument, rejecting a wrong-typed one instead of
/// silently treating it as absent.
fn arg_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("argument '{key}' must be a string")),
    }
}

fn arg_i64(args: &Map<String, Value>, key: &str) -> Result<Option<i64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("argument '{key}' must be an integer")),
        Some(_) => Err(format!("argument '{key}' must be an integer")),
    }
}

fn arg_bool(args: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("argument '{key}' must be a boolean")),
    }
}

fn arg_f64(args: &Map<String, Value>, key: &str) -> Result<Option<f64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("argument '{key}' must be a number")),
        Some(_) => Err(format!("argument '{key}' must be a number")),
    }
}

/// The one piece of pre-processing a structured argument needs: `ladder`
/// (ladder design §6, §8 WP6) and `lmgw__settings_set`'s `realtime`
/// (realtime design §12). The schema declares each a JSON-encoded string
/// (`ladder_p`'s doc), but the patch field is already typed —
/// `LocalModelPatch.ladder` an `Option<Vec<Rung>>`, `SettingsPatch.realtime`
/// an `Option<RealtimeSettingsPatch>`, not `Value` — so unlike
/// `capabilities_override` there is no later point where a string still
/// parses into the right shape. So this runs before `ops::patch_from_args`'s
/// generic deserialize, turning a JSON string into the real value it decodes
/// to; a caller that sends the array or object directly anyway is accepted
/// too, the same leniency `parse_capabilities_override` extends the other way
/// (an object where a string is declared). Absent or already non-string is
/// left alone — that includes an array, an object, `Null` and a caller's
/// mistake, which `patch_from_args` then reports on its own.
fn hoist_json_arg(args: &mut Map<String, Value>, key: &str) -> Result<(), String> {
    let Some(Value::String(s)) = args.get(key) else {
        return Ok(());
    };
    let s = s.trim();
    if s.is_empty() {
        // Every other optional string in this module means "leave unchanged"
        // when empty (`ops::opt`), and an agent that fills every optional
        // field with "" is common with flat-string schemas (review T3) — so
        // an empty `ladder` must not silently clear an existing one.
        // Clearing stays explicit: `clear: "ladder"`, or `ladder: "[]"`.
        args.remove(key);
        return Ok(());
    }
    let parsed: Value =
        serde_json::from_str(s).map_err(|e| format!("{key}: invalid JSON ({e})"))?;
    args.insert(key.to_string(), parsed);
    Ok(())
}

/// The Backends tools whose scalar arguments are read one by one
/// (`arg_*`), and so would otherwise drop an argument they do not know:
/// their closed schema is enforced here, so a typo (`offest`) is refused by
/// name like a patch tool's `deny_unknown_fields` refuses it, instead of
/// running with the default. (`lmgw__build_set` goes through a patch struct
/// that already refuses.)
const CLOSED_ARG_TOOLS: [&str; 8] = [
    "lmgw__builds",
    "lmgw__build_log",
    "lmgw__container_images",
    "lmgw__forge_prs",
    "lmgw__build_run",
    "lmgw__build_check_merge",
    "lmgw__container_image_delete",
    "lmgw__container_image_pull",
];

/// An argument `def` does not declare, as the refusal that names it and the
/// ones it takes.
fn unknown_arg(def: &Builtin, args: Option<&Map<String, Value>>) -> Option<String> {
    let known: Vec<&str> = def.props.iter().map(|(k, _)| *k).collect();
    let bad = args?.keys().find(|k| !known.contains(&k.as_str()))?;
    Some(format!(
        "unknown argument '{bad}' for {} — it takes {}",
        def.name,
        if known.is_empty() {
            "no arguments".to_string()
        } else {
            known.join(", ")
        }
    ))
}

/// Whether the built-in tool `name` writes: needs [`SelfAdmin::Full`].
/// Every tool that has lmgw run a program on this machine is one — an MCP
/// server's command or container, a model's or a build's container, an
/// agent — so a caller below `Full` reaches none of them. `false` for a name
/// that is no tool here.
pub fn writes(name: &str) -> bool {
    catalog().iter().any(|t| t.name == name && t.writes)
}

/// Why a device whose own level is `level` may not call the write tool
/// `name` (the pre-merge review's P-3): its level, not the gateway's, is
/// what stops it.
pub fn device_level_refusal(name: &str, level: crate::config::DeviceAdmin) -> String {
    format!(
        "{name} changes lmgw's configuration and this device's admin tools are {} — the \
         device's level is set on its row under Usage → Devices",
        match level {
            crate::config::DeviceAdmin::Off => "off",
            crate::config::DeviceAdmin::ReadOnly => "read only",
            crate::config::DeviceAdmin::Full => "full",
        }
    )
}

/// Invoke a built-in tool as the owner: [`call_capped`] at the gateway's
/// own level.
pub async fn call(
    state: &SharedState,
    name: &str,
    args: Option<Map<String, Value>>,
) -> Result<Value, CallError> {
    call_capped(state, name, args, Caller::OWNER).await
}

/// Who makes a built-in tool call, as far as [`call_capped`]'s gate asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    /// A paired device's key: its own level caps the call, an agent run it
    /// starts carries it as the run's caller, and an agent it did not create
    /// is not its to replace or delete (client-apps design L5's note,
    /// 2026-10-07). `None` for the owner and every caller that is no device.
    pub device: Option<i64>,
}

impl Caller {
    /// The owner, and every caller that is no device.
    pub const OWNER: Self = Self { device: None };

    /// The caller `principal` is.
    pub fn of(principal: &crate::principal::Principal) -> Self {
        Self {
            device: principal
                .is_device_key()
                .then(|| principal.key_id())
                .flatten(),
        }
    }
}

/// A device's own level and the gateway's, as stored now rather than as
/// the published snapshot says (client-apps design L3's note; the branch
/// review's verification V-7): a lowered level is in force from its commit
/// for every call a device makes, its turns' (which `ScopedExecutor` checks
/// too) and those of an agent run it started alike. A device that is gone,
/// disabled or expired is `Off`.
async fn stored_levels(
    state: &SharedState,
    device: i64,
) -> Result<(SelfAdmin, SelfAdmin), crate::error::GatewayError> {
    let own = crate::store::device_admin_now(&state.db, device).await?;
    let gateway = crate::store::gateway_self_admin_now(&state.db).await?;
    Ok((own.as_self_admin(), gateway))
}

/// Invoke a built-in tool for `caller`: the mode gate applies the lower of
/// a device's own level and the gateway's (both as stored now, for a
/// device; the gateway's as published, for anyone else), and names the
/// device's level when that is what refuses. Then, for every caller, the access
/// settings are refused ([`guards`]). A move of a stored credential's host
/// without the credential is refused by the op that applies it
/// (`ops::RowWriter::Tool`).
///
/// `Err(CallError)` is reserved for "this name is not a tool" — everything else,
/// including a mode refusal or a bad argument, comes back as an `isError`
/// result the calling model can read and act on.
pub async fn call_capped(
    state: &SharedState,
    name: &str,
    args: Option<Map<String, Value>>,
    caller: Caller,
) -> Result<Value, CallError> {
    let Some(def) = catalog().into_iter().find(|t| t.name == name) else {
        return Err(CallError::ToolNotFound(name.to_string()));
    };

    let (cap, global) = match caller.device {
        Some(device) => match stored_levels(state, device).await {
            Ok(levels) => levels,
            Err(e) => {
                return Ok(err_result(&format!(
                    "{name} — whether this device may use lmgw's admin tools could not be read \
                     ({e}), so the call was not made"
                )))
            }
        },
        None => (SelfAdmin::Full, state.snapshot().settings.self_admin),
    };
    if let Err(e) = ops::check_mode(global.min(cap), def.writes) {
        // The device's own level is the lower one: say that, not the
        // gateway's Setting, which would not let it through either way.
        let e = if cap < global {
            match cap {
                SelfAdmin::Off => format!("{name} — this device is not allowed lmgw's admin tools"),
                SelfAdmin::ReadOnly => {
                    device_level_refusal(name, crate::config::DeviceAdmin::ReadOnly)
                }
                SelfAdmin::Full => e,
            }
        } else {
            e
        };
        return Ok(err_result(&e));
    }
    if let Some(e) = guards::access_refusal(name, args.as_ref()) {
        return Ok(err_result(&e));
    }
    if CLOSED_ARG_TOOLS.contains(&def.name) {
        if let Some(e) = unknown_arg(&def, args.as_ref()) {
            return Ok(err_result(&e));
        }
    }

    Ok(match run(state, name, args, caller).await {
        Ok(v) => ok_result(&v),
        Err(e) => err_result(&e),
    })
}

/// The name → [`ops`] mapping proper, with argument extraction. Split out so
/// [`call`] owns only gating and result shaping. `caller` reaches the agent
/// tools only: a device's run is its own, and so is what it may replace.
async fn run(
    state: &SharedState,
    name: &str,
    args: Option<Map<String, Value>>,
    caller: Caller,
) -> Result<Value, String> {
    let a = args.clone().unwrap_or_default();
    match name {
        "lmgw__status" => ops::status(state).await,
        "lmgw__models" => {
            let kind = arg_str(&a, "kind")?;
            let search = arg_str(&a, "search")?;
            ops::models(state, kind, search).await
        }
        "lmgw__upstreams" => ops::upstreams(state).await,
        "lmgw__agents" => ops::agents(state).await,
        "lmgw__agent_get" => {
            ops::agent_get(state, arg_str(&a, "id")?.ok_or("id is required")?).await
        }
        "lmgw__agent_set" => {
            // Not `arg_str`: an object is accepted too, with the ordering
            // warning `manifest_arg` attaches to it.
            ops::agent_set(
                state,
                a.get("manifest").ok_or("manifest is required")?,
                arg_bool(&a, "replace")?,
                arg_bool(&a, "validate_only")?,
                caller.device,
            )
            .await
        }
        "lmgw__agent_install" => {
            ops::agent_install(
                state,
                arg_str(&a, "image")?.ok_or("image is required")?,
                arg_str(&a, "pull")?,
                arg_bool(&a, "replace")?,
                arg_bool(&a, "validate_only")?,
                caller.device,
            )
            .await
        }
        "lmgw__agent_run" => {
            ops::agent_run(
                state,
                arg_str(&a, "id")?.ok_or("id is required")?,
                arg_str(&a, "phase")?.ok_or("phase is required")?,
                caller.device,
            )
            .await
        }
        "lmgw__agent_delete" => {
            ops::agent_delete(
                state,
                arg_str(&a, "id")?.ok_or("id is required")?,
                caller.device,
            )
            .await
        }
        "lmgw__mcp_servers" => ops::mcp_servers(state).await,
        "lmgw__logs" => {
            ops::logs(
                state,
                arg_i64(&a, "limit")?,
                arg_bool(&a, "errors_only")?,
                arg_str(&a, "alias")?,
                arg_i64(&a, "before_id")?,
            )
            .await
        }
        "lmgw__settings" => ops::settings(state).await,
        "lmgw__local_model_get" => {
            ops::local_model_get(
                state,
                arg_i64(&a, "id")?,
                arg_str(&a, "model_id")?,
                arg_str(&a, "target")?,
            )
            .await
        }
        "lmgw__local_model_check" => {
            ops::local_model_check(state, arg_str(&a, "model_id")?, arg_str(&a, "target")?).await
        }
        "lmgw__gguf_files" => {
            crate::modelinfo::gguf_files(state, arg_str(&a, "search")?, arg_str(&a, "target")?)
                .await
        }
        "lmgw__model_inspect" => {
            let path = arg_str(&a, "gguf_path")?.ok_or("gguf_path is required")?;
            crate::modelinfo::model_inspect(
                state,
                path,
                arg_bool(&a, "probe")?.unwrap_or(true),
                arg_str(&a, "target")?,
            )
            .await
        }
        "lmgw__local_model_plan" => {
            let path = arg_str(&a, "gguf_path")?.ok_or("gguf_path is required")?;
            crate::modelinfo::local_model_plan(
                state,
                path,
                arg_bool(&a, "probe")?.unwrap_or(true),
                arg_str(&a, "target")?,
            )
            .await
        }
        "lmgw__local_model_test" => {
            let mid = arg_str(&a, "model_id")?.ok_or("model_id is required")?;
            crate::modelinfo::local_model_test(state, mid, arg_str(&a, "target")?).await
        }
        "lmgw__llama_flags" => {
            ops::llama_flags(state, arg_str(&a, "search")?, arg_str(&a, "model")?).await
        }
        "lmgw__hf_repo" => {
            let repo = arg_str(&a, "repo")?.ok_or("repo is required (owner/name)")?;
            ops::hf_repo(
                state,
                repo,
                arg_str(&a, "search")?,
                arg_str(&a, "target")?.unwrap_or("chat"),
            )
            .await
        }
        "lmgw__hf_downloads" => ops::hf_downloads(state).await,
        "lmgw__image_recipes" => ops::image_recipes(state).await,
        "lmgw__image_recipe_add" => {
            let key = arg_str(&a, "key")?.ok_or("key is required (see lmgw__image_recipes)")?;
            ops::image_recipe_add(state, key, arg_str(&a, "diffusion_file")?).await
        }
        "lmgw__usage" => {
            ops::usage(
                state,
                arg_str(&a, "from")?,
                arg_str(&a, "to")?,
                arg_str(&a, "group_by")?,
                arg_str(&a, "class")?,
                arg_str(&a, "alias")?,
                arg_i64(&a, "limit")?,
            )
            .await
        }
        "lmgw__prices" => ops::prices(state).await,
        "lmgw__hf_add" => {
            let repo = arg_str(&a, "repo")?.ok_or("repo is required (owner/name)")?;
            ops::hf_add(
                state,
                repo,
                arg_str(&a, "file")?,
                arg_str(&a, "quant")?,
                arg_str(&a, "target")?.unwrap_or("chat"),
                arg_bool(&a, "companions")?.unwrap_or(true),
            )
            .await
        }
        "lmgw__hf_set" => {
            let action = arg_str(&a, "action")?
                .ok_or("action is required (redownload|delete|check_updates)")?;
            ops::hf_set(
                state,
                action,
                arg_i64(&a, "id")?,
                arg_str(&a, "target")?.unwrap_or("chat"),
            )
            .await
        }
        "lmgw__upstream_set" => {
            ops::upstream_set(state, ops::patch_from_args(args)?, ops::RowWriter::Tool).await
        }
        "lmgw__model_set" => ops::model_set(state, ops::patch_from_args(args)?).await,
        "lmgw__candidate_alias_set" => {
            ops::candidate_alias_set(state, ops::patch_from_args(args)?).await
        }
        "lmgw__local_model_set" => {
            let mut a = args.unwrap_or_default();
            hoist_json_arg(&mut a, "ladder")?;
            ops::local_model_set(state, ops::patch_from_args(Some(a))?).await
        }
        "lmgw__aux_model_set" => ops::aux_model_set(state, ops::patch_from_args(args)?).await,
        "lmgw__image_model_set" => ops::image_model_set(state, ops::patch_from_args(args)?).await,
        "lmgw__mcp_server_set" => {
            ops::mcp_server_set(state, ops::patch_from_args(args)?, ops::RowWriter::Tool).await
        }
        "lmgw__container" => {
            let target = arg_str(&a, "target")?;
            let model = arg_str(&a, "model")?;
            let action = arg_str(&a, "action")?
                .ok_or("action is required (status|start|stop|restart|apply|logs)")?;
            let force = arg_bool(&a, "override")?.unwrap_or(false);
            let tail = arg_i64(&a, "tail")?;
            ops::container(state, target, model, action, force, tail).await
        }
        "lmgw__hold_set" => {
            let active = arg_bool(&a, "active")?.ok_or("active is required (true|false)")?;
            ops::hold_set(state, active).await
        }
        "lmgw__settings_set" => {
            let mut a = args.unwrap_or_default();
            hoist_json_arg(&mut a, "realtime")?;
            ops::settings_set(state, ops::patch_from_args(Some(a))?).await
        }
        "lmgw__prices_sync" => ops::prices_sync(state).await,
        "lmgw__price_set" => {
            let scope_kind = arg_str(&a, "scope_kind")?
                .ok_or("scope_kind is required (alias|upstream_model)")?;
            ops::price_set(
                state,
                scope_kind,
                arg_str(&a, "scope_key")?,
                arg_str(&a, "unit")?,
                ops::PriceRates {
                    price_in: arg_f64(&a, "price_in")?,
                    price_out: arg_f64(&a, "price_out")?,
                    price_cache_read: arg_f64(&a, "price_cache_read")?,
                    price_cache_write: arg_f64(&a, "price_cache_write")?,
                    price: arg_f64(&a, "price")?,
                },
                arg_str(&a, "note")?,
            )
            .await
        }
        "lmgw__price_delete" => {
            let id = arg_i64(&a, "id")?.ok_or("id is required")?;
            ops::price_delete(state, id).await
        }
        "lmgw__docs_corpora" => ops::docs_corpora(state, arg_str(&a, "corpus")?).await,
        "lmgw__docs_requests" => ops::docs_requests(state, arg_str(&a, "status")?).await,
        "lmgw__docs_corpus_set" => ops::docs_corpus_set(state, ops::patch_from_args(args)?).await,
        "lmgw__docs_ingest" => {
            ops::docs_ingest(
                state,
                arg_str(&a, "corpus")?,
                arg_str(&a, "action")?.unwrap_or_default(),
                arg_str(&a, "embed_model")?,
            )
            .await
        }
        "lmgw__docs_request_set" => {
            ops::docs_request_set(
                state,
                arg_i64(&a, "id")?,
                arg_str(&a, "status")?.unwrap_or_default(),
            )
            .await
        }
        "lmgw__builds" => {
            ops::backends::builds_tool(
                state,
                arg_i64(&a, "id")?,
                arg_i64(&a, "limit")?,
                arg_i64(&a, "before")?,
            )
            .await
        }
        "lmgw__build_log" => {
            let run_id = arg_i64(&a, "run_id")?.ok_or("run_id is required")?;
            ops::backends::build_log_tool(
                state,
                run_id,
                arg_i64(&a, "offset")?,
                arg_i64(&a, "tail")?,
            )
            .await
        }
        "lmgw__container_images" => {
            let engine = arg_str(&a, "engine")?
                .map(|e| {
                    lmgw_api_types::builds::Engine::parse(e)
                        .ok_or_else(|| format!("engine '{e}' is not one of llama, audio, sdcpp"))
                })
                .transpose()?;
            ops::backends::to_json(
                ops::backends::container_images(
                    state,
                    lmgw_api_types::builds::ContainerImagesArgs { engine, disk: true },
                )
                .await,
            )
        }
        "lmgw__forge_prs" => {
            ops::backends::forge_prs_tool(
                state,
                arg_i64(&a, "id")?,
                arg_str(&a, "repo_url")?,
                arg_str(&a, "forge")?,
                arg_str(&a, "query")?,
                arg_i64(&a, "page")?,
            )
            .await
        }
        "lmgw__build_set" => ops::backends::build_patch(state, ops::patch_from_args(args)?).await,
        "lmgw__build_run" => {
            let id = arg_i64(&a, "id")?.ok_or("id is required (see lmgw__builds)")?;
            let rebuild = arg_bool(&a, "rebuild")?.unwrap_or(false);
            let started = ops::backends::build_run(
                state,
                lmgw_api_types::builds::BuildRunArgs { id, rebuild },
                lmgw_api_types::builds::BuildTrigger::Mcp,
            )
            .await?;
            let mut v = ops::backends::to_json(Ok(started))?;
            v["next_step"] = serde_json::json!(format!(
                "follow the output with lmgw__build_log run_id={} offset=0 (then each \
                 next_offset), and the outcome with lmgw__builds id={id} until live_job_id is \
                 null",
                started.run_id
            ));
            Ok(v)
        }
        "lmgw__build_check_merge" => {
            let id = arg_i64(&a, "id")?.ok_or("id is required (see lmgw__builds)")?;
            ops::backends::to_json(
                ops::backends::build_check_merge(
                    state,
                    lmgw_api_types::builds::BuildCheckMergeArgs {
                        id: Some(id),
                        spec: None,
                    },
                )
                .await,
            )
        }
        "lmgw__container_image_delete" => {
            let image = arg_str(&a, "image")?.ok_or("image is required (a tag or an ID)")?;
            ops::backends::to_json(
                ops::backends::container_image_delete(
                    state,
                    lmgw_api_types::builds::ContainerImageDeleteArgs {
                        image: image.to_string(),
                        force: arg_bool(&a, "force")?.unwrap_or(false),
                    },
                )
                .await,
            )
        }
        "lmgw__container_image_pull" => {
            let image = arg_str(&a, "image")?.ok_or("image is required (a registry reference)")?;
            let started = ops::backends::container_image_pull(
                state,
                lmgw_api_types::builds::ContainerImagePullArgs {
                    image: image.to_string(),
                },
            )
            .await?;
            let mut v = ops::backends::to_json(Ok(started.clone()))?;
            v["next_step"] = serde_json::json!(format!(
                "the pull runs in the background (job {}); lmgw__container_images shows the new \
                 image and registry_update.update_available=false once it is done. Containers of \
                 the old image keep it until recreated: lmgw__container model=<id> action=apply",
                started.job_id
            ));
            Ok(v)
        }
        bench if bench.starts_with("lmgw__bench_") => self::bench::run(state, bench, args).await,
        "lmgw__audio_catalog" | "lmgw__audio_model_set" | "lmgw__voice_transcribe" => {
            self::audio::run(state, name, args).await
        }
        other => Err(format!("unhandled built-in tool '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_is_namespaced_and_within_the_name_ceiling() {
        for t in catalog() {
            assert!(owns(t.name), "{} must carry the reserved prefix", t.name);
            assert!(
                t.name.len() <= super::super::MAX_TOOL_NAME_LEN,
                "{} exceeds the exposed-name ceiling",
                t.name
            );
        }
    }

    #[test]
    fn tool_names_are_unique() {
        let mut names: Vec<&str> = catalog().iter().map(|t| t.name).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate built-in tool name");
    }

    /// The constraint that shapes this whole module: a small local model has to
    /// be able to fill these in, which means no hand-built JSON arguments.
    #[test]
    fn all_parameters_are_flat_scalars() {
        for t in catalog() {
            for (key, spec) in &t.props {
                let ty = spec["type"].as_str().unwrap_or("");
                assert!(
                    matches!(ty, "string" | "integer" | "boolean" | "number"),
                    "{}.{key} has non-scalar type '{ty}' — tool arguments must stay flat",
                    t.name
                );
            }
        }
    }

    #[test]
    fn required_arguments_are_declared_properties() {
        for t in catalog() {
            for req in t.required {
                assert!(
                    t.props.iter().any(|(k, _)| k == req),
                    "{} requires '{req}' but never declares it",
                    t.name
                );
            }
        }
    }

    #[test]
    fn listing_follows_the_mode_gate() {
        assert!(list(SelfAdmin::Off).is_empty());

        let ro = list(SelfAdmin::ReadOnly);
        let full = list(SelfAdmin::Full);
        assert_eq!(full.len(), catalog().len());
        assert!(ro.len() < full.len());

        // Read-only must expose no tool flagged as mutating.
        let writers: Vec<&str> = catalog()
            .iter()
            .filter(|t| t.writes)
            .map(|t| t.name)
            .collect();
        for entry in &ro {
            let name = entry["name"].as_str().unwrap();
            assert!(!writers.contains(&name), "{name} leaked into read_only");
        }
        assert!(ro.iter().any(|e| e["name"] == "lmgw__status"));
    }

    #[test]
    fn entries_carry_a_closed_object_schema() {
        for entry in list(SelfAdmin::Full) {
            let schema = &entry["inputSchema"];
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], json!(false));
            assert!(entry["description"].as_str().is_some_and(|d| d.len() > 20));
        }
    }

    /// `lmgw__build_set preset=` spells the preset ids out as an enum (a
    /// description is `&'static str`); this keeps it in step with the table.
    #[test]
    fn build_set_offers_exactly_the_repo_presets() {
        let t = catalog()
            .into_iter()
            .find(|t| t.name == "lmgw__build_set")
            .unwrap();
        let (_, preset) = t.props.iter().find(|(k, _)| *k == "preset").unwrap();
        let offered: Vec<&str> = preset["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let table: Vec<&str> = crate::backends::presets::REPO_PRESETS
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(offered, table);
    }

    /// `lmgw__audio_model_set task=` spells audio.cpp's task names out as an
    /// enum; this keeps it in step with the list a save validates against.
    #[test]
    fn audio_model_set_offers_exactly_the_audio_tasks() {
        let t = catalog()
            .into_iter()
            .find(|t| t.name == "lmgw__audio_model_set")
            .unwrap();
        let (_, task) = t.props.iter().find(|(k, _)| *k == "task").unwrap();
        let offered: Vec<&str> = task["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(offered, crate::web::audio::AUDIO_TASKS.to_vec());
    }

    /// The description states the default tail and the offset chunk size;
    /// this keeps both in step with the constants behind them.
    #[test]
    fn build_log_states_its_tail_default_and_chunk_size() {
        let t = catalog()
            .into_iter()
            .find(|t| t.name == "lmgw__build_log")
            .unwrap();
        let tail = crate::ops::backends::TOOL_LOG_TAIL;
        assert!(t.description.contains(&format!("LAST {tail} LINES")));
        assert_eq!(crate::backends::run::LOG_CHUNK_BYTES, 1024 * 1024);
        assert!(t.description.contains("at most 1 MiB per call"));
        assert!(t.props.iter().any(|(k, _)| *k == "tail"));
    }

    #[test]
    fn closed_arg_tools_refuse_an_argument_they_do_not_declare() {
        let cat = catalog();
        for name in CLOSED_ARG_TOOLS {
            assert!(cat.iter().any(|t| t.name == name), "{name} is not a tool");
        }
        let log = cat.iter().find(|t| t.name == "lmgw__build_log").unwrap();
        let a: Map<String, Value> = serde_json::from_str(r#"{"run_id": 1, "offest": 5}"#).unwrap();
        assert_eq!(
            unknown_arg(log, Some(&a)).as_deref(),
            Some("unknown argument 'offest' for lmgw__build_log — it takes run_id, offset, tail")
        );
        let ok: Map<String, Value> = serde_json::from_str(r#"{"run_id": 1, "tail": 5}"#).unwrap();
        assert_eq!(unknown_arg(log, Some(&ok)), None);
        assert_eq!(unknown_arg(log, None), None);
    }

    #[test]
    fn owns_only_claims_the_reserved_namespace() {
        assert!(owns("lmgw__status"));
        assert!(!owns("gh__search"));
        assert!(!owns("lmgw_status"));
        assert!(!owns("search"));
    }

    #[test]
    fn scalar_arg_extraction_rejects_wrong_types() {
        let a: Map<String, Value> = serde_json::from_str(
            r#"{"limit": 10, "alias": "gpt", "errors_only": true, "bad": []}"#,
        )
        .unwrap();
        assert_eq!(arg_i64(&a, "limit").unwrap(), Some(10));
        assert_eq!(arg_str(&a, "alias").unwrap(), Some("gpt"));
        assert_eq!(arg_bool(&a, "errors_only").unwrap(), Some(true));
        assert_eq!(arg_i64(&a, "missing").unwrap(), None);
        assert!(arg_str(&a, "limit").is_err());
        assert!(arg_i64(&a, "bad").is_err());
    }
}
