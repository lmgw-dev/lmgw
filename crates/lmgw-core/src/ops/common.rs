//! Shared parsing for the newline-delimited list fields.
//!
//! Also home to the self-loop guard, candidate-alias name uniqueness
//! helpers, and `patch_from_args`/`check_mode` -- every `ops` child module
//! reaches these through `super::`.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::capabilities::{self, Derived, ModelCapabilities};
use crate::config::{SelfAdmin, Snapshot, UpstreamKind};
use crate::runtime::Class;

use super::*;

/// Placeholder substituted for any stored secret on the way out. A distinct,
/// obviously-not-a-value marker so a caller can tell "configured" from "empty"
/// without ever seeing the secret.
pub const REDACTED: &str = "<set>";

/// Parse a `KEY=VALUE`-per-line block into env pairs. Shared with the MCP tab's
/// form (`web::mcp`) so the admin UI and the tool plane accept the identical
/// syntax.
pub fn parse_env(raw: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| format!("invalid env line (expected KEY=VALUE): {line}"))?;
        out.push((k.trim().to_string(), v.trim().to_string()));
    }
    Ok(out)
}

/// Parse a `Name: Value`-per-line block into header pairs (shared with
/// `web::mcp`, as [`parse_env`]).
pub fn parse_headers(raw: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (n, v) = line
            .split_once(':')
            .ok_or_else(|| format!("invalid header line (expected Name: Value): {line}"))?;
        out.push((n.trim().to_string(), v.trim().to_string()));
    }
    Ok(out)
}

/// A host → secret map with every value replaced by [`REDACTED`] — which hosts
/// have a token, never what it is. The `forge_tokens` setting's read shape on
/// every surface (container-builds §7).
pub fn redact_map(map: &std::collections::BTreeMap<String, String>) -> Value {
    Value::Object(
        map.keys()
            .map(|host| (host.clone(), Value::from(REDACTED)))
            .collect(),
    )
}

/// Render pairs back out with their values redacted, in the same
/// newline-delimited syntax the parsers accept — so a read shows *which* env
/// vars / headers are set without leaking the tokens in them.
pub(super) fn redact_pairs(pairs: &[(String, String)], sep: &str) -> String {
    pairs
        .iter()
        .map(|(k, _)| format!("{k}{sep}{REDACTED}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Trim, then `None` if empty — the patch-field idiom (`Some("")` from a model
/// that fills every argument means "unset", not "set to empty").
pub(super) fn opt(s: &Option<String>) -> Option<String> {
    s.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Parse and shape-check a `capabilities_override` patch value (model-
/// capabilities design §7) before it is written: a JSON **object**, or (MCP
/// tool arguments are flat scalars, so a caller there has no way to send a
/// nested object) a JSON string containing one — mirrors how
/// `chat_template_kwargs` is accepted. `null`, an empty string, or a missing
/// value all mean "no override, clear it" and come back `Ok(None)`.
///
/// Shape-checked here, not just at `/v1/models` time, by running it through
/// [`capabilities::apply_owner_override`] against a minimal stand-in base (a
/// bare `chat` capabilities object) — the exact merge/deserialise rules that
/// will apply for real later, so a malformed override is refused on write
/// with a message naming the offending key rather than silently accepted and
/// only surfacing (or being silently dropped) on the next model listing.
pub fn parse_capabilities_override(v: &Value) -> Result<Option<Value>, String> {
    let obj = match v {
        Value::Null => return Ok(None),
        Value::String(s) if s.trim().is_empty() => return Ok(None),
        Value::String(s) => serde_json::from_str::<Value>(s.trim())
            .map_err(|e| format!("capabilities_override: invalid JSON ({e})"))?,
        other => other.clone(),
    };
    let check_base = Derived {
        capabilities: Some(ModelCapabilities {
            task: "chat".to_string(),
            endpoints: Vec::new(),
            source: "gguf+config".to_string(),
            ..Default::default()
        }),
        ..Derived::default()
    };
    capabilities::apply_owner_override(check_base, &obj)?;
    Ok(Some(obj))
}

/// Report **all** absent required fields at once.
///
/// Validating in declaration order and returning at the first gap makes a
/// caller discover the requirements one failed call at a time — tolerable for a
/// human filling in a form, wasteful for a model that has to re-plan each round
/// trip. One message naming everything missing is a single correction.
pub(super) fn require_all(missing: &[(&str, bool)]) -> Result<(), String> {
    let absent: Vec<&str> = missing
        .iter()
        .filter(|(_, present)| !present)
        .map(|(name, _)| *name)
        .collect();
    if absent.is_empty() {
        return Ok(());
    }
    Err(format!("create requires: {}", absent.join(", ")))
}

/// Reject an empty path or one that escapes the dir it will be joined onto
/// (`..`, an absolute path, `.`, a Windows prefix) — shared by every model
/// domain that stores a path relative to a configured models dir.
pub fn check_rel_path(path: &str, what: &str) -> Result<(), String> {
    let bad = std::path::Path::new(path)
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)));
    if path.is_empty() || bad {
        Err(format!(
            "'{path}' must be a plain path relative to the {what}"
        ))
    } else {
        Ok(())
    }
}

/// The field names a `clear` argument lists, comma- or whitespace-separated.
///
/// Whole names, never substrings: `clear=extra_run_args` must not also empty
/// `args`, which is exactly what a `contains` test read it as — one patch
/// field's name is another's suffix in three of the four model patches.
pub(super) fn clear_names(clear: Option<&str>) -> impl Iterator<Item = &str> {
    clear
        .unwrap_or_default()
        .split([',', ' ', '\n', '\t', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Whether a `clear` argument names `field` (see [`clear_names`]).
pub(crate) fn clear_has(clear: Option<&str>, field: &str) -> bool {
    clear_names(clear).any(|n| n == field)
}

/// A caller-supplied GGUF path as stored: the in-container `/models/` prefix
/// (what the rendered command line shows, and what a caller copying one back
/// naturally pastes) and any leading slash are dropped.
pub(super) fn strip_models_prefix(path: &str) -> String {
    path.trim()
        .trim_start_matches("/models/")
        .trim_start_matches('/')
        .to_string()
}

/// Which class a model id belongs to, given what the caller said about it.
///
/// `model_id` is unique per table only (per-model-containers §3.3), so a
/// chat and an aux model can share one; `given` (from a `target` argument)
/// settles that, and is otherwise only checked for consistency. The error
/// texts name the fix, since the caller is usually an agent with no other way
/// to find out which table a name lives in.
pub(crate) fn resolve_model_class(
    snap: &Snapshot,
    model_id: &str,
    given: Option<Class>,
) -> Result<Class, String> {
    let matches = find_model_classes(snap, model_id);
    match matches.len() {
        0 => Err(format!(
            "no model named '{model_id}' is configured in any class"
        )),
        1 => {
            let found = matches[0];
            match given {
                Some(g) if g != found => Err(format!(
                    "target '{}' does not match model '{model_id}', which is a {found} model",
                    g.as_str()
                )),
                _ => Ok(found),
            }
        }
        _ => match given {
            Some(g) if matches.contains(&g) => Ok(g),
            _ => Err(format!(
                "model '{model_id}' exists in more than one class ({}) — pass target to \
                 disambiguate",
                matches
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        },
    }
}

/// An owner-supplied `upstreams.kind`.
///
/// Every spelling [`UpstreamKind::parse`] knows **except `sd_cpp`**: that one
/// exists only on the synthetic image upstream, which is built in memory and
/// never written, so `upstreams.kind`'s CHECK constraint does not list it.
/// Accepting it here would turn a typo into a raw SQLite constraint failure
/// with no explanation; refusing it says what the owner actually wants
/// instead.
pub(super) fn parse_upstream_kind(s: &str) -> Result<UpstreamKind, String> {
    match UpstreamKind::parse(s) {
        Some(UpstreamKind::SdCpp) | None => Err(format!(
            "invalid kind '{s}' (generic|llama_server|audio_cpp). Local image models are rows in \
             the image class, not upstreams."
        )),
        Some(kind) => Ok(kind),
    }
}

/// `target` as the tools spell it (`chat` | `aux` | `audio`, plus the old
/// `embed` for aux) to a class, or `None` when absent.
pub(crate) fn parse_class_target(target: Option<&str>) -> Result<Option<Class>, String> {
    match target.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some("chat") => Ok(Some(Class::Chat)),
        Some("aux") | Some("embed") => Ok(Some(Class::Aux)),
        Some("audio") => Ok(Some(Class::Audio)),
        Some("image") => Ok(Some(Class::Image)),
        Some(other) => Err(format!("unknown target '{other}' (chat|aux|audio|image)")),
    }
}

// Self-loop guard

/// Reject registering an MCP server that points back at **this** gateway's own
/// northbound `/mcp`.
///
/// Without this, one `mcp_server_set` call makes lmgw its own upstream: a
/// `tools/list` aggregates its own aggregate, and every `lmgw__*` call recurses
/// until a transport dies. It was always possible to do this by hand from the
/// dashboard, but making server registration agent-drivable turns a typo into a
/// self-inflicted loop, so the check lives here — in the layer both front ends
/// share — rather than in the caller.
///
/// Matching is host+port+path: any loopback spelling (`localhost`, `127.0.0.1`,
/// `[::1]`) counts as our host, and a wildcard bind (`0.0.0.0` / `::`) matches
/// every host on the configured port, since all of them reach us.
pub fn reject_self_loop(snap: &Snapshot, url: &str) -> Result<(), String> {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return Ok(()); // not a URL we can judge; the transport will complain
    };
    // **The aggregate endpoint exactly**, not any path ending in `/mcp`
    // (container-runtime §3.3). `/agents/<id>/mcp` is this gateway's *proxy* to
    // an agent's own container: registering it is the intended shape, and
    // aggregating it recurses into nothing. Narrowing this is also the more
    // correct rule for every other nested path a future plane might serve.
    if !matches!(parsed.path(), "/mcp" | "/mcp/") {
        return Ok(());
    }
    let bind = &snap.settings.bind_addr;
    let (bind_host, bind_port) = match bind.rsplit_once(':') {
        Some((h, p)) => (h.trim_matches(['[', ']']), p),
        None => return Ok(()),
    };
    let Ok(bind_port) = bind_port.parse::<u16>() else {
        return Ok(());
    };
    let Some(url_port) = parsed.port_or_known_default() else {
        return Ok(());
    };
    if url_port != bind_port {
        return Ok(());
    }

    let is_loopback = |h: &str| matches!(h, "localhost" | "127.0.0.1" | "::1" | "[::1]");
    let wildcard = matches!(bind_host, "0.0.0.0" | "::" | "");
    let url_host = parsed.host_str().unwrap_or("");
    if wildcard || url_host == bind_host || (is_loopback(url_host) && is_loopback(bind_host)) {
        return Err(format!(
            "'{url}' is this gateway's own MCP endpoint (bind address {bind}) — \
             registering it would make lmgw aggregate itself, recursing on every \
             tools/list and tools/call"
        ));
    }
    Ok(())
}

/// `agent:` is the agent lifecycle's own namespace in `mcp_servers.name`
/// (container-runtime §3.3).
///
/// An owner-created row called `agent:foo` would be silently adopted — and
/// deleted — by the next write to an agent with that id, so it is refused with
/// that reason rather than accepted and later mourned.
/// The `timeout_ms` a **new** MCP server row gets when nobody names one.
///
/// One constant rather than a literal per creation path: the MCP page's create
/// form and the `agent:<id>` row the agent lifecycle writes (container-runtime
/// §3.3) have to agree about what "the default" is, and an agent's row
/// inventing its own number would be a bound nobody chose.
pub const DEFAULT_MCP_TIMEOUT_MS: u64 = 60_000;

pub fn reject_agent_name(name: &str) -> Result<(), String> {
    if name.starts_with(crate::agents::service::MCP_NAME_PREFIX) {
        return Err(format!(
            "'{name}' starts with the reserved '{}' prefix: rows named that way belong to an \
             agent's `run.provides.mcp` and are created, updated and deleted with the agent. \
             Pick another name.",
            crate::agents::service::MCP_NAME_PREFIX
        ));
    }
    Ok(())
}

// Candidate-alias name uniqueness (candidate-aliases design §4.1)

/// Refuse `name` when it is already a **candidate alias** (case-insensitive,
/// any stored row — enabled or not, since a disabled one still holds its
/// name). Called from the four other name-introducing save paths (a plain
/// alias, and a local/aux/audio/image row's create or rename) so the two
/// kinds of alias cannot shadow each other.
///
/// Deliberately one-directional here: whether a plain alias and a local row's
/// public name can already collide with *each other* is a pre-existing gap
/// (nothing enforces that today either), and closing it is out of this
/// phase's scope. [`candidate_alias_set`] enforces the other three
/// directions when a candidate alias itself is named.
pub(crate) fn refuse_if_candidate_alias_name(snap: &Snapshot, name: &str) -> Result<(), String> {
    if snap.candidate_aliases.contains_key(&name.to_lowercase()) {
        return Err(format!(
            "'{name}' is already a candidate alias — pick another name, or rename/delete that \
             candidate alias first (lmgw__candidate_alias_set)"
        ));
    }
    Ok(())
}

/// Whether `name` is already claimed by one of the other three name spaces a
/// candidate alias must stay clear of (candidate-aliases design §4.1): a
/// plain alias, a local public name of any of the four classes, or another
/// candidate alias. `exclude_id` is the row being saved, on an update — its
/// own current name must not collide with itself.
pub(super) fn candidate_alias_name_taken(
    snap: &Snapshot,
    name: &str,
    exclude_id: Option<i64>,
) -> Option<&'static str> {
    let key = name.to_lowercase();
    // `Snapshot.aliases` is keyed by the alias's literal case (`store/snapshot.rs`'s
    // `snap.aliases.insert(a.alias.clone(), a)`), unlike `candidate_aliases`
    // below — a lowercased-key lookup here almost never matched, so a
    // candidate alias could silently shadow a mixed-case plain alias
    // (review finding X4). Candidate-alias lookup (`Snapshot::candidate_alias`,
    // `Snapshot::resolve`) is case-insensitive, so this check must be too, in
    // both directions.
    if snap.aliases.keys().any(|k| k.eq_ignore_ascii_case(name)) {
        return Some("a plain alias");
    }
    if snap.local_models.iter().any(|m| {
        snap.local_public_name(&m.model_id)
            .eq_ignore_ascii_case(name)
    }) {
        return Some("a local chat model");
    }
    if snap
        .aux_models
        .iter()
        .any(|m| snap.aux_public_name(&m.model_id).eq_ignore_ascii_case(name))
    {
        return Some("an aux model");
    }
    if snap.audio_models.iter().any(|m| {
        snap.audio_public_name(&m.model_id)
            .eq_ignore_ascii_case(name)
    }) {
        return Some("an audio model");
    }
    if snap.image_models.iter().any(|m| {
        snap.image_public_name(&m.model_id)
            .eq_ignore_ascii_case(name)
    }) {
        return Some("an image model");
    }
    if snap
        .candidate_aliases
        .get(&key)
        .is_some_and(|c| Some(c.id) != exclude_id)
    {
        return Some("another candidate alias");
    }
    None
}

/// Deserialize a flat tool-argument map into a patch struct, turning serde's
/// message into something a model can act on.
pub fn patch_from_args<T: for<'de> Deserialize<'de>>(
    args: Option<Map<String, Value>>,
) -> Result<T, String> {
    let value = Value::Object(args.unwrap_or_default());
    serde_json::from_value(value).map_err(|e| format!("invalid arguments: {e}"))
}

/// Whether a mode permits a given tool class, with an error naming the setting
/// so a refused call tells the caller exactly what to change.
pub fn check_mode(mode: SelfAdmin, needs_write: bool) -> Result<(), String> {
    match (mode, needs_write) {
        (SelfAdmin::Off, _) => Err(
            "lmgw self-admin tools are disabled (Self-admin tools is off under Settings → \
                 Network & access)"
                .to_string(),
        ),
        (SelfAdmin::ReadOnly, true) => Err(
            "this tool mutates gateway configuration and self-admin is set to read_only — \
             set Self-admin tools under Settings → Network & access to 'full' to allow it"
                .to_string(),
        ),
        _ => Ok(()),
    }
}
