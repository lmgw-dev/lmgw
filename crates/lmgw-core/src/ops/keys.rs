//! Keys & policy (usage-analytics design §4.1)

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{ApiKey, ApiKeyKind, BudgetPeriod, KeyPolicy, ScopeMode};
use crate::state::SharedState;
use crate::store::{self};

/// The **owner-set** half of one key's policy. Sparse like every other patch:
/// a field left out keeps the value the row already has.
///
/// The other half is *derived* — an agent token's scope and enabled flag are
/// written by lmgw from the agent's manifest and row (container-runtime §3.1)
/// — and [`key_set`] refuses to change those rather than writing a value the
/// next resync would silently take back.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct KeyPatch {
    pub id: i64,
    pub enabled: Option<bool>,
    pub scope_mode: Option<String>,
    pub scope_patterns: Option<String>,
    /// The MCP tool scope: the same modes and glob syntax, over exposed tool
    /// names.
    pub tool_scope_mode: Option<String>,
    pub tool_scope_patterns: Option<String>,
    pub budget_micro: Option<i64>,
    pub budget_period: Option<String>,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub concurrency_limit: Option<i64>,
    /// `YYYY-MM-DD` — which means the **whole** of that day, the same reading
    /// `policy::expired` applies — or an RFC3339 timestamp. `""` clears it.
    pub expires_at: Option<String>,
    pub note: Option<String>,
}

fn parse_scope_mode(v: &str) -> Result<ScopeMode, String> {
    parse_mode_field("scope_mode", v)
}

/// A mode in its wire spelling, refused under the name of the field that
/// carried it — `scope_mode` and `tool_scope_mode` share the vocabulary.
fn parse_mode_field(field: &str, v: &str) -> Result<ScopeMode, String> {
    match v.trim() {
        "all" => Ok(ScopeMode::All),
        "allow" => Ok(ScopeMode::Allow),
        "deny" => Ok(ScopeMode::Deny),
        other => Err(format!("unknown {field} '{other}' (all|allow|deny)")),
    }
}

fn parse_budget_period(v: &str) -> Result<BudgetPeriod, String> {
    match v.trim() {
        "day" => Ok(BudgetPeriod::Day),
        "month" => Ok(BudgetPeriod::Month),
        "total" => Ok(BudgetPeriod::Total),
        other => Err(format!("unknown budget_period '{other}' (day|month|total)")),
    }
}

/// One glob per line, trimmed, blanks dropped — so "same list, retyped with a
/// trailing newline" is not a change and does not read as one.
fn normalize_patterns(v: &str) -> String {
    v.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// An expiry lmgw will still understand when it matters.
///
/// Validated **here, where it is typed**, rather than at the door: `policy`
/// deliberately ignores an unparseable date (refusing on a typo would lock an
/// owner out of their own gateway), which means a typo saved here would read
/// as "expiry set" on the page and expire nothing, forever.
fn validate_expiry(v: &str) -> Result<Option<String>, String> {
    let v = v.trim();
    if v.is_empty() {
        return Ok(None);
    }
    if chrono::DateTime::parse_from_rfc3339(v).is_ok()
        || chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").is_ok()
    {
        return Ok(Some(v.to_string()));
    }
    Err(format!(
        "'{v}' is not a date lmgw reads — use YYYY-MM-DD (the whole of that day) or an \
         RFC3339 timestamp, or leave it empty for no expiry"
    ))
}

/// Refusal for the two columns an agent token does not own.
fn refuse_derived(key: &ApiKey, what: &str) -> String {
    match what {
        "enabled" => format!(
            "'{}' is an agent token: its enabled flag mirrors the agent row, which is what makes \
             Disable on the agent's page a kill switch for the token everywhere at once. Toggle \
             the agent there instead.",
            key.name
        ),
        "tool scope" => format!(
            "'{}' is an agent token: the tools it may see and call are its manifest's tools[], \
             resolved against what the gateway offers, and that is the list /mcp and \
             /v1/responses apply to it. Change the manifest on the agent's page.",
            key.name
        ),
        _ => format!(
            "'{}' is an agent token: its scope is derived from the agent's manifest — every \
             model_alias config field, plus a literal model.alias — and is rewritten on the next \
             save, run or import of that agent. Change the model on the agent's page and the \
             token's scope follows it. Budget, rate limits, expiry and the note are yours to set \
             here.",
            key.name
        ),
    }
}

/// Refusal for the columns an internal identity carries but nothing reads.
fn refuse_inert(key: &ApiKey, what: &str) -> String {
    if what == "enabled" {
        return format!(
            "'{}' is an internal identity: it can never authenticate anything, so enabling or \
             disabling the row changes nothing at all. Its levers are scope and budget.",
            key.name
        );
    }
    if what == "tool scope" {
        return format!(
            "'{}' is an internal identity: work lmgw does on its own behalf attaches its tools \
             in process and never presents a credential, so a tool scope here would be stored \
             and never read. Its levers are scope and budget.",
            key.name
        );
    }
    format!(
        "'{}' is an internal identity. {what} is checked in the auth middleware, and work lmgw \
         does on its own behalf never passes through it — a value set here would be stored and \
         never read. Its levers are scope and budget, both of which are enforced.",
        key.name
    )
}

/// Refusal for switching off the one credential that could switch it back on
/// (principals §3.1, §3.12). Shared with `key_delete`, which refuses the same
/// row for the same reason — delete is disable that cannot be undone.
pub const REFUSE_DOOR: &str = "the dashboard key is the door; rotate it instead";

/// Refusal for the client-shaped half of the dialog on an owner row
/// (principals §3.12).
pub const REFUSE_OWNER_POLICY: &str =
    "an owner key is not a client; it is not scoped, budgeted or rate-limited";

/// The owner half of [`key_set`]'s rules (principals §3.12), as the refusal
/// message or nothing.
///
/// Its own function rather than four more branches inside [`key_set`] because
/// the `/api` plane needs the same decision one step earlier: an owner refusal
/// answers with the `refuse_owner` code (design §7) instead of the flat
/// `op_failed` every other op-level error gets, and a code is attached in
/// `web`, where every other code is. `key_set` still calls it, so the rule
/// holds for any caller and not only for the dashboard's.
///
/// A field counts as an *attempt* only when its value differs from the one the
/// row already carries — the same reading `key_set` applies to a derived
/// field, and for the same reason: a front end that posts its whole form is
/// refused for what it tried to change and for nothing else.
pub fn owner_key_refusal(key: &ApiKey, p: &KeyPatch) -> Option<String> {
    if key.kind != ApiKeyKind::Owner {
        return None;
    }
    if p.enabled.is_some_and(|v| v != key.enabled)
        && key.name == crate::agents::token::OWNER_DASHBOARD
    {
        return Some(REFUSE_DOOR.to_string());
    }
    let policy = &key.policy;
    let touched = p
        .scope_mode
        .as_deref()
        .is_some_and(|v| parse_scope_mode(v) != Ok(policy.scope_mode))
        || p.scope_patterns
            .as_deref()
            .is_some_and(|v| normalize_patterns(v) != policy.scope_patterns)
        || p.tool_scope_mode
            .as_deref()
            .is_some_and(|v| parse_mode_field("tool_scope_mode", v) != Ok(policy.tool_scope_mode))
        || p.tool_scope_patterns
            .as_deref()
            .is_some_and(|v| normalize_patterns(v) != policy.tool_scope_patterns)
        || p.budget_micro.is_some_and(|v| v != policy.budget_micro)
        || p.budget_period
            .as_deref()
            .is_some_and(|v| parse_budget_period(v) != Ok(policy.budget_period))
        || p.rpm_limit.is_some_and(|v| v != policy.rpm_limit)
        || p.tpm_limit.is_some_and(|v| v != policy.tpm_limit)
        || p.concurrency_limit
            .is_some_and(|v| v != policy.concurrency_limit)
        || p.expires_at.as_deref().is_some_and(|v| {
            validate_expiry(v) != Ok(policy.expires_at.clone().filter(|e| !e.is_empty()))
        });
    touched.then(|| REFUSE_OWNER_POLICY.to_string())
}

/// The scope half of a `key_create`, checked **before** the row exists: a
/// scope that cannot be applied must refuse the create, not leave a key behind
/// that is wide open because its scope failed to land afterwards.
///
/// `Ok(true)` means the patch carries a scope that differs from the default
/// (`all`, no patterns) — the same "attempt" reading [`owner_key_refusal`]
/// applies, so an owner create that restates the default is not refused.
pub fn validate_create_scope(p: &KeyPatch) -> Result<bool, String> {
    let mut non_default = false;
    if let Some(v) = p.scope_mode.as_deref() {
        non_default |= parse_scope_mode(v)? != ScopeMode::All;
    }
    if let Some(v) = p.tool_scope_mode.as_deref() {
        non_default |= parse_mode_field("tool_scope_mode", v)? != ScopeMode::All;
    }
    for v in [
        p.scope_patterns.as_deref(),
        p.tool_scope_patterns.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        non_default |= !normalize_patterns(v).is_empty();
    }
    Ok(non_default)
}

/// Write one key's policy (usage-analytics §4.1).
///
/// Every field is compared against the stored value before it is applied, so a
/// front end that posts the whole form on every save — which is what the
/// dashboard's dialog does — is refused only for a field it actually tried to
/// change. Restating a derived value is not an attempt to change it.
pub async fn key_set(state: &SharedState, p: KeyPatch) -> Result<Value, String> {
    let snap = state.snapshot();
    let key: ApiKey = snap
        .api_keys
        .iter()
        .find(|k| k.id == p.id)
        .cloned()
        .ok_or_else(|| {
            format!(
                "no key with id {} — pass the id the Keys table (or lmgw__usage) shows",
                p.id
            )
        })?;

    // An owner row's rules first, so that the field-by-field walk below only
    // ever sees the two fields an owner key actually has (§3.12).
    if let Some(refusal) = owner_key_refusal(&key, &p) {
        return Err(refusal);
    }

    let mut policy: KeyPolicy = key.policy.clone();
    let mut enabled = key.enabled;
    let mut note = key.note.clone();
    let mut changed: Vec<&str> = Vec::new();
    let derived = key.kind == ApiKeyKind::Agent;
    let inert = key.kind == ApiKeyKind::Internal;

    if let Some(v) = p.enabled {
        if v != enabled {
            if derived {
                return Err(refuse_derived(&key, "enabled"));
            }
            if inert {
                return Err(refuse_inert(&key, "enabled"));
            }
            enabled = v;
            changed.push("enabled");
        }
    }

    if let Some(v) = p.scope_mode.as_deref() {
        let mode = parse_scope_mode(v)?;
        if mode != policy.scope_mode {
            if derived {
                return Err(refuse_derived(&key, "scope_mode"));
            }
            policy.scope_mode = mode;
            changed.push("scope_mode");
        }
    }
    if let Some(v) = p.scope_patterns.as_deref() {
        let patterns = normalize_patterns(v);
        if patterns != policy.scope_patterns {
            if derived {
                return Err(refuse_derived(&key, "scope_patterns"));
            }
            policy.scope_patterns = patterns;
            changed.push("scope_patterns");
        }
    }

    // The tool scope binds a credential a client presents. An agent's tools
    // are its manifest's, and an internal identity presents nothing.
    if let Some(v) = p.tool_scope_mode.as_deref() {
        let mode = parse_mode_field("tool_scope_mode", v)?;
        if mode != policy.tool_scope_mode {
            if derived {
                return Err(refuse_derived(&key, "tool scope"));
            }
            if inert {
                return Err(refuse_inert(&key, "tool scope"));
            }
            policy.tool_scope_mode = mode;
            changed.push("tool_scope_mode");
        }
    }
    if let Some(v) = p.tool_scope_patterns.as_deref() {
        let patterns = normalize_patterns(v);
        if patterns != policy.tool_scope_patterns {
            if derived {
                return Err(refuse_derived(&key, "tool scope"));
            }
            if inert {
                return Err(refuse_inert(&key, "tool scope"));
            }
            policy.tool_scope_patterns = patterns;
            changed.push("tool_scope_patterns");
        }
    }

    if let Some(v) = p.budget_micro {
        if v < 0 {
            return Err("budget_micro cannot be negative — 0 means no budget".to_string());
        }
        if v != policy.budget_micro {
            policy.budget_micro = v;
            changed.push("budget_micro");
        }
    }
    if let Some(v) = p.budget_period.as_deref() {
        let period = parse_budget_period(v)?;
        if period != policy.budget_period {
            policy.budget_period = period;
            changed.push("budget_period");
        }
    }

    for (label, supplied, current) in [
        ("rpm_limit", p.rpm_limit, policy.rpm_limit),
        ("tpm_limit", p.tpm_limit, policy.tpm_limit),
        (
            "concurrency_limit",
            p.concurrency_limit,
            policy.concurrency_limit,
        ),
    ] {
        let Some(v) = supplied else { continue };
        if v < 0 {
            return Err(format!("{label} cannot be negative — 0 means no limit"));
        }
        if v == current {
            continue;
        }
        if inert {
            return Err(refuse_inert(&key, "a rate limit"));
        }
        match label {
            "rpm_limit" => policy.rpm_limit = v,
            "tpm_limit" => policy.tpm_limit = v,
            _ => policy.concurrency_limit = v,
        }
        changed.push(label);
    }

    if let Some(v) = p.expires_at.as_deref() {
        let expiry = validate_expiry(v)?;
        if expiry != policy.expires_at.clone().filter(|e| !e.is_empty()) {
            if inert {
                return Err(refuse_inert(&key, "expiry"));
            }
            policy.expires_at = expiry;
            changed.push("expires_at");
        }
    }

    if let Some(v) = p.note.as_deref() {
        if v.trim() != note {
            note = v.trim().to_string();
            changed.push("note");
        }
    }

    if changed.is_empty() {
        return Ok(json!({
            "ok": true,
            "id": key.id,
            "name": key.name,
            "changed": changed,
            "message": format!("'{}' already reads exactly that — nothing written", key.name),
        }));
    }

    let rows = store::update_key_policy(&state.db, key.id, &policy, enabled, &note)
        .await
        .map_err(|e| e.to_string())?;
    if rows == 0 {
        return Err(format!(
            "key {} disappeared between reading it and writing it — nothing was changed",
            key.id
        ));
    }
    state.reload_snapshot().await.map_err(|e| e.to_string())?;
    // A session this key holds on `/mcp` has its own copy of `tools/list`;
    // nudge it to re-list, as a per-tool switch does.
    if changed.iter().any(|c| c.starts_with("tool_scope")) {
        state.mcp.notify_tools_changed_now();
    }
    Ok(json!({
        "ok": true,
        "id": key.id,
        "name": key.name,
        "changed": changed,
        "message": format!("'{}': updated {}", key.name, changed.join(", ")),
    }))
}
