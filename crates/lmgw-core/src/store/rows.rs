//! Row → domain mapping

use sqlx::Row;

use crate::config::{
    ApiKey, ApiKeyKind, BudgetPeriod, CandidateAlias, HoldFallbackMode, KeyPolicy, LocalModel,
    McpServer, McpTransport, ModelAlias, PriceRow, PriceScope, PriceUnit, Protocol, ScopeMode,
    Upstream, UpstreamKind,
};
use crate::error::GatewayError;
use crate::ir::Params;
use crate::pricing::PriceSource;

use super::*;

pub(super) fn parse_json_or<T: serde::de::DeserializeOwned + Default>(s: &str) -> T {
    serde_json::from_str(s).unwrap_or_default()
}

pub(super) fn to_json<T: serde::Serialize>(v: &T) -> DbResult<String> {
    serde_json::to_string(v).map_err(|e| GatewayError::Internal(e.to_string()))
}

/// A nullable JSON column, e.g. `image`/`extra_run_args`'s NULL-means-inherit
/// encoding on `local_models`/`aux_models`/`audio_models` (per-model-containers
/// design §6): the SQL column itself is the `Option`, so unlike
/// [`parse_json_or`] there is no "malformed reads as absent" fallback to hide
/// behind — a NULL column is `None`, anything else must parse.
pub(super) fn parse_json_opt<T: serde::de::DeserializeOwned>(s: Option<String>) -> Option<T> {
    s.and_then(|s| serde_json::from_str(&s).ok())
}

pub(super) fn to_json_opt<T: serde::Serialize>(v: &Option<T>) -> DbResult<Option<String>> {
    v.as_ref().map(to_json).transpose()
}

/// A per-model `extra_run_args` column: NULL inherits the class's run args,
/// and so does an empty list — never stored as `[]`, which would start the
/// container without the class's GPU and SELinux flags. The ops plane
/// already folds an empty override into "none" (`ops::run_args_override`);
/// this keeps every other writer to the same rule.
pub(super) fn run_args_json(v: &Option<Vec<String>>) -> DbResult<Option<String>> {
    to_json_opt(&v.as_ref().filter(|a| !a.is_empty()))
}

/// A stored `capabilities_override` (model-capabilities design §7): free-form
/// JSON the owner wrote, so it is read back as a `Value` rather than a typed
/// struct.
///
/// Text that is not JSON at all is kept as `Value::String(raw)` instead of
/// being dropped: `capabilities::apply_owner_override` then refuses it and the
/// model's own `notes` carry that refusal, which is the whole point of the
/// escape hatch. Reading it as `None` would be indistinguishable from "no
/// override configured" — the owner would see their override silently not
/// applied, with nothing anywhere saying why.
pub(super) fn capabilities_override_from_row(raw: Option<String>) -> Option<serde_json::Value> {
    let raw = raw?;
    Some(serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw)))
}

pub(super) fn upstream_from_row(row: &sqlx::sqlite::SqliteRow) -> Upstream {
    Upstream {
        id: row.get("id"),
        name: row.get("name"),
        protocol: Protocol::parse(row.get::<String, _>("protocol").as_str())
            .unwrap_or(Protocol::Openai),
        kind: UpstreamKind::parse(row.get::<String, _>("kind").as_str())
            .unwrap_or(UpstreamKind::Generic),
        base_url: row.get("base_url"),
        api_key: row.get("api_key"),
        extra_headers: parse_json_or(row.get::<String, _>("extra_headers").as_str()),
        timeout_ms: row.get::<i64, _>("timeout_ms").max(0) as u64,
        enabled: row.get::<i64, _>("enabled") != 0,
        expose_all: row.get::<i64, _>("expose_all") != 0,
        expose_prefix: row.get("expose_prefix"),
        supports_responses: row.get::<i64, _>("supports_responses") != 0,
        llama: None,
    }
}

pub(super) fn alias_from_row(row: &sqlx::sqlite::SqliteRow) -> ModelAlias {
    ModelAlias {
        id: row.get("id"),
        alias: row.get("alias"),
        upstream_id: row.get("upstream_id"),
        upstream_model_id: row.get("upstream_model_id"),
        param_overrides: parse_json_or::<Params>(row.get::<String, _>("param_overrides").as_str()),
        enabled: row.get::<i64, _>("enabled") != 0,
        capabilities_override: capabilities_override_from_row(
            row.get::<Option<String>, _>("capabilities_override"),
        ),
    }
}

/// Read `hold_fallback_mode` back out, defaulting to `Inherit` on anything
/// this build does not recognise — same "unwrap_or(default)" convention as
/// `Protocol`/`UpstreamKind` (see `McpTransport`'s doc comment).
pub(super) fn hold_fallback_mode_from_row(row: &sqlx::sqlite::SqliteRow) -> HoldFallbackMode {
    row.get::<String, _>("hold_fallback_mode")
        .parse()
        .unwrap_or(HoldFallbackMode::Inherit)
}

/// Same convention as [`hold_fallback_mode_from_row`], for
/// `candidate_aliases.fallback_mode` (candidate-aliases design §4.1).
fn candidate_fallback_mode_from_row(row: &sqlx::sqlite::SqliteRow) -> HoldFallbackMode {
    row.get::<String, _>("fallback_mode")
        .parse()
        .unwrap_or(HoldFallbackMode::Inherit)
}

pub(super) fn candidate_alias_from_row(row: &sqlx::sqlite::SqliteRow) -> CandidateAlias {
    CandidateAlias {
        id: row.get("id"),
        alias: row.get("alias"),
        candidates: parse_json_or(row.get::<String, _>("candidates").as_str()),
        background: row.get::<i64, _>("background") != 0,
        fallback_mode: candidate_fallback_mode_from_row(row),
        fallback: row.get("fallback"),
        capabilities_disabled: parse_json_or(
            row.get::<String, _>("capabilities_disabled").as_str(),
        ),
        capabilities_enabled: parse_json_or(row.get::<String, _>("capabilities_enabled").as_str()),
        enabled: row.get::<i64, _>("enabled") != 0,
        notes: row.get("notes"),
    }
}

pub(super) fn local_model_from_row(row: &sqlx::sqlite::SqliteRow) -> LocalModel {
    let mut m = LocalModel {
        id: row.get("id"),
        model_id: row.get("model_id"),
        gguf_path: row.get("gguf_path"),
        params: parse_json_or(row.get::<String, _>("params").as_str()),
        args: parse_json_or(row.get::<String, _>("args").as_str()),
        idle_seconds: row.get("idle_seconds"),
        enabled: row.get::<i64, _>("enabled") != 0,
        public: row.get::<i64, _>("public") != 0,
        image: row.get("image"),
        extra_run_args: parse_json_opt(row.get::<Option<String>, _>("extra_run_args")),
        warm_start: row.get::<i64, _>("warm_start") != 0,
        hold_fallback_mode: hold_fallback_mode_from_row(row),
        hold_fallback: row.get("hold_fallback"),
        capabilities_override: capabilities_override_from_row(
            row.get::<Option<String>, _>("capabilities_override"),
        ),
        ladder: parse_json_or(row.get::<String, _>("ladder").as_str()),
    };
    // Flags that gained a dedicated field after this row was written still sit
    // in `args`; fold them in so every reader sees one representation.
    m.hoist_promoted_args();
    m
}

pub(super) fn api_key_from_row(row: &sqlx::sqlite::SqliteRow) -> ApiKey {
    ApiKey {
        id: row.get("id"),
        name: row.get("name"),
        key_hash: row.get("key_hash"),
        enabled: row.get::<i64, _>("enabled") != 0,
        kind: ApiKeyKind::parse(row.get::<String, _>("kind").as_str()),
        key_plain: row
            .get::<Option<String>, _>("key_plain")
            .map(crate::config::Secret::new),
        agent_id: row.get("agent_id"),
        policy: KeyPolicy {
            scope_mode: ScopeMode::parse(row.get::<String, _>("scope_mode").as_str()),
            scope_patterns: row.get("scope_patterns"),
            tool_scope_mode: ScopeMode::parse(row.get::<String, _>("tool_scope_mode").as_str()),
            tool_scope_patterns: row.get("tool_scope_patterns"),
            budget_micro: row.get("budget_micro"),
            budget_period: BudgetPeriod::parse(row.get::<String, _>("budget_period").as_str()),
            rpm_limit: row.get("rpm_limit"),
            tpm_limit: row.get("tpm_limit"),
            concurrency_limit: row.get("concurrency_limit"),
            expires_at: row.get("expires_at"),
        },
        note: row.get("note"),
        hosts_label: row.get("hosts_label"),
        self_admin: crate::config::DeviceAdmin::from_column(row.get::<i64, _>("self_admin")),
    }
}

pub(super) fn price_from_row(row: &sqlx::sqlite::SqliteRow) -> PriceRow {
    PriceRow {
        id: row.get("id"),
        scope_kind: PriceScope::parse(row.get::<String, _>("scope_kind").as_str()),
        scope_key: row.get("scope_key"),
        // The table's CHECK admits only the units `PriceUnit` names.
        unit: PriceUnit::parse(row.get::<String, _>("unit").as_str()).unwrap_or_default(),
        price_in: row.get("price_in"),
        price_out: row.get("price_out"),
        price_cache_read: row.get("price_cache_read"),
        price_cache_write: row.get("price_cache_write"),
        source: PriceSource::parse(row.get::<String, _>("source").as_str()),
        note: row.get("note"),
        updated_at: row.get("updated_at"),
        price: row.get("price"),
    }
}

pub(super) fn mcp_server_from_row(row: &sqlx::sqlite::SqliteRow) -> McpServer {
    McpServer {
        id: row.get("id"),
        name: row.get("name"),
        enabled: row.get::<i64, _>("enabled") != 0,
        transport: McpTransport::parse(row.get::<String, _>("transport").as_str())
            .unwrap_or(McpTransport::Stdio),
        command: row.get("command"),
        args: parse_json_or(row.get::<String, _>("args").as_str()),
        env: parse_json_or(row.get::<String, _>("env").as_str()),
        cwd: row.get("cwd"),
        container_image: row.get("container_image"),
        extra_run_args: parse_json_or(row.get::<String, _>("extra_run_args").as_str()),
        url: row.get("url"),
        headers: parse_json_or(row.get::<String, _>("headers").as_str()),
        tool_prefix: row.get("tool_prefix"),
        timeout_ms: row.get::<i64, _>("timeout_ms").max(0) as u64,
        autostart: row.get::<i64, _>("autostart") != 0,
        idle_seconds: row.get("idle_seconds"),
        allow_sampling: row.get::<i64, _>("allow_sampling") != 0,
        sampling_alias: row.get("sampling_alias"),
        agent_id: row.get("agent_id"),
        device_key_id: row.get("device_key_id"),
    }
}
