//! Hand-written argument schemas (api-docs design §4.7 table D): a worker
//! read the dispatch arm or the function body it calls and wrote down exactly
//! the keys it reads — these are not generated from a Rust type, because none
//! of these ops parses its `Map<String, Value>` into one (`ops/common.rs`'s
//! `patch_from_args` is for a typed patch; these read individual keys with
//! `arg_str`/`args.get(..)` instead).
//!
//! Each function returns the JSON Schema object itself (an
//! `OpArgs::Hand(fn() -> Value)` schema, §4.7's `OpArgs` doc comment) —
//! `"type": "object"`, `properties`, `required`, `additionalProperties:
//! false`, matching the shape every other op's schema carries.

use serde_json::{json, Value};

use lmgw_api_types::openapi_ext;

/// `{"type":"object","properties":..,"required":..,"additionalProperties":false}`,
/// the one shape every Hand schema below returns.
fn obj(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn str_p(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}
fn int_p(desc: &str) -> Value {
    json!({ "type": "integer", "description": desc })
}
fn bool_p(desc: &str) -> Value {
    json!({ "type": "boolean", "description": desc })
}
fn enum_p(desc: &str, values: &[&str]) -> Value {
    json!({ "type": "string", "enum": values, "description": desc })
}

/// An agent's config `values` patch (`agent_config_set`, `agent_open_chat`,
/// `agent_run`), flagged `x-lmgw-secret` as a whole: a manifest's `secret`
/// fields (an IMAP password, an API token) travel in the same object as the
/// ordinary ones, and nothing in this schema can say which keys those are
/// (review R2 #3).
fn config_values_p(desc: &str) -> Value {
    json!({ "type": "object", "description": desc, (openapi_ext::SECRET): true })
}

// ---------------------------------------------------------------------------
// Main dispatcher (web/api.rs `fn op`)
// ---------------------------------------------------------------------------

/// `tool_set { name*, enabled* }` — `web/api.rs`'s `"tool_set"` arm.
pub(crate) fn tool_set() -> Value {
    obj(
        json!({
            "name": str_p("The fully-qualified tool name (e.g. 'gh__search'), as `/api/tools` lists it."),
            "enabled": bool_p("Whether the tool is offered to callers."),
        }),
        &["name", "enabled"],
    )
}

/// `job_cancel { id* }` — `web/api.rs`'s `"job_cancel"` arm
/// (`ops::job_cancel`).
pub(crate) fn job_cancel() -> Value {
    obj(
        json!({ "id": int_p("The job id, from /api/jobs or the SSE `jobs` frame.") }),
        &["id"],
    )
}

/// `response_chain_delete { chain_id* }` — `web/api.rs`'s
/// `"response_chain_delete"` arm.
pub(crate) fn response_chain_delete() -> Value {
    obj(
        json!({ "chain_id": str_p("The stored-response chain to delete, from /api/responses.") }),
        &["chain_id"],
    )
}

/// `responses_gc { scope }` — `web/api.rs`'s `"responses_gc"` arm.
/// `scope` absent means `rules` (the retention settings' own eviction);
/// `all` clears every stored response regardless of age.
pub(crate) fn responses_gc() -> Value {
    obj(
        json!({
            "scope": enum_p(
                "'rules' (default) evicts by the retention settings; 'all' clears every \
                 stored response.",
                &["rules", "all"],
            ),
        }),
        &[],
    )
}

/// `upstream_test { id* }` — `web/api.rs`'s `"upstream_test"` arm: a live
/// connectivity check against one configured upstream.
pub(crate) fn upstream_test() -> Value {
    obj(
        json!({ "id": int_p("Upstream id, from /api/upstreams.") }),
        &["id"],
    )
}

/// `audio_catalog { action*, family, package }` — `web/api.rs`'s
/// `audio_catalog_op`: `refresh` re-fetches the audio.cpp model-spec catalog
/// (no other argument); `download` queues one package's files and requires
/// both `family` and `package`.
pub(crate) fn audio_catalog() -> Value {
    obj(
        json!({
            "action": enum_p(
                "'refresh' live-fetches the audio.cpp model_specs catalog; 'download' queues \
                 the files one package lacks.",
                &["refresh", "download"],
            ),
            "family": str_p("download only: the catalog family (e.g. 'kokoro')."),
            "package": str_p("download only: the package within that family."),
        }),
        &["action"],
    )
}

/// `price_set` (§4.7 table B divergence: `scope_kind` defaults to `alias`
/// here, where the tool requires it) — `web/api.rs`'s `"price_set"` arm
/// (`ops::price_set`).
pub(crate) fn price_set() -> Value {
    obj(
        json!({
            "scope_kind": enum_p(
                "What scope_key names. Default 'alias'.",
                &["alias", "upstream_model"],
            ),
            "scope_key": str_p(
                "The alias name for scope_kind=alias, or '<upstream_id>:<upstream_model_id>' \
                 for scope_kind=upstream_model — /api/upstream-models lists existing \
                 scope_keys.",
            ),
            "price_in": json!({ "type": "number", "description": "Input price per 1M tokens." }),
            "price_out": json!({ "type": "number", "description": "Output price per 1M tokens." }),
            "price_cache_read": json!({
                "type": "number",
                "description": "Cache-read price per 1M tokens. Omit to bill at price_in.",
            }),
            "price_cache_write": json!({
                "type": "number",
                "description": "Cache-write price per 1M tokens. Omit to bill at price_in.",
            }),
            "note": str_p("Free-text note, e.g. where this number came from."),
        }),
        &["scope_key"],
    )
}

// ---------------------------------------------------------------------------
// Credential ops (web/api_settings.rs `fn key_op`)
// ---------------------------------------------------------------------------

/// `key_create { name*, kind }` — a client key (default) or an owner key.
pub(crate) fn key_create() -> Value {
    obj(
        json!({
            "name": str_p("Display name for the new key."),
            "kind": enum_p("Default 'client'.", &["client", "owner"]),
            "scope_mode": enum_p(
                "Client keys only: which model aliases the key may ask for. Default 'all'.",
                &["all", "allow", "deny"],
            ),
            "scope_patterns": str_p(
                "Newline-delimited `*` globs over alias names, for scope_mode allow/deny.",
            ),
            "tool_scope_mode": enum_p(
                "Client keys only: which MCP tools the key may see and call. Default 'all'.",
                &["all", "allow", "deny"],
            ),
            "tool_scope_patterns": str_p(
                "Newline-delimited `*` globs over exposed tool names (e.g. `docs__*`, \
                 `github__search`), for tool_scope_mode allow/deny.",
            ),
        }),
        &["name"],
    )
}

/// `key_delete { id* }`.
pub(crate) fn key_delete() -> Value {
    obj(
        json!({ "id": int_p("Key id, from the Keys table or /api/usage/keys.") }),
        &["id"],
    )
}

/// `key_reveal { id* }`.
pub(crate) fn key_reveal() -> Value {
    obj(
        json!({ "id": int_p("Owner key id, from the Keys table.") }),
        &["id"],
    )
}

/// `key_rotate { id* }`.
pub(crate) fn key_rotate() -> Value {
    obj(
        json!({ "id": int_p("Owner key id, from the Keys table.") }),
        &["id"],
    )
}

// ---------------------------------------------------------------------------
// Agent catalog (web/api_agents.rs `fn op`)
// ---------------------------------------------------------------------------

/// `agent_duplicate { id*, new_id*, name }`.
pub(crate) fn agent_duplicate() -> Value {
    obj(
        json!({
            "id": str_p("Agent id to copy, from /api/agents."),
            "new_id": str_p("The copy's id — a DNS label ([a-z0-9][a-z0-9-]*)."),
            "name": str_p("The copy's display name. Default: '<name> (copy)'."),
        }),
        &["id", "new_id"],
    )
}

/// `agent_config_set { id*, values*, clear }` — a sparse patch over the
/// agent's stored config, plus the one way to remove a value outright.
pub(crate) fn agent_config_set() -> Value {
    obj(
        json!({
            "id": str_p("Agent id, from /api/agents."),
            "values": config_values_p(
                "A sparse patch over the stored config values — a field left out keeps what \
                 is stored.",
            ),
            "clear": json!({
                "type": "array",
                "items": { "type": "string" },
                "description": "Field names to remove from the stored config outright (the one \
                    gesture for 'forget this value', including a secret an empty submission \
                    would otherwise keep).",
            }),
        }),
        &["id", "values"],
    )
}

/// `agent_dev_url_set { id*, url }` — point the agent's app at a dev server,
/// or (omitted/empty) back at its image.
pub(crate) fn agent_dev_url_set() -> Value {
    obj(
        json!({
            "id": str_p("Agent id, from /api/agents."),
            "url": str_p("The dev server's URL. Omit or empty to serve from the image again."),
        }),
        &["id"],
    )
}

/// `agent_enable { id*, enabled* }`.
pub(crate) fn agent_enable() -> Value {
    obj(
        json!({
            "id": str_p("Agent id, from /api/agents."),
            "enabled": bool_p("Whether the agent (and its token) is usable at all."),
        }),
        &["id", "enabled"],
    )
}

/// `agent_open_chat { id*, values }` — open (or reuse) a chat thread for a
/// `chat`-kind agent.
pub(crate) fn agent_open_chat() -> Value {
    obj(
        json!({
            "id": str_p("Agent id, from /api/agents."),
            "values": config_values_p(
                "A sparse patch over the stored config, applied to this run only — same shape \
                 as agent_config_set's values.",
            ),
        }),
        &["id"],
    )
}

/// `agent_run { id*, phase*, rows, base_job, values }` (§4.7 table B
/// divergence: the tool only starts `list`/`classify`, not `apply`, and has
/// no `rows`/`base_job`/`values`). `run` (the container runtime's own phase)
/// is a real `batch::Phase` value but is refused wherever it is parsed —
/// never reachable through this op.
pub(crate) fn agent_run() -> Value {
    obj(
        json!({
            "id": str_p("Agent id, from /api/agents."),
            "phase": enum_p(
                "What to run.",
                &["list", "classify", "rerun", "apply"],
            ),
            "rows": json!({
                "type": "array",
                "items": {},
                "description": "apply only: the reviewed rows from a finished 'classify' run.",
            }),
            "base_job": int_p("apply only: the job id whose config this apply reuses."),
            "values": config_values_p(
                "A sparse patch over the stored config, applied to this run only.",
            ),
        }),
        &["id", "phase"],
    )
}

/// `agent_service_log { id*, lines }`.
pub(crate) fn agent_service_log() -> Value {
    obj(
        json!({
            "id": str_p("Agent id, from /api/agents."),
            "lines": int_p("How many lines from the end. 0 = the whole log. Default a short excerpt."),
        }),
        &["id"],
    )
}

/// `agent_pull` / `agent_reimport` / `agent_reset` / `agent_run_cancel` /
/// `agent_token_get` / `agent_token_rotate` / `agent_service_start` /
/// `agent_service_stop` — every agent op that takes only `{ id }`.
pub(crate) fn agent_id_only() -> Value {
    obj(
        json!({ "id": str_p("Agent id, from /api/agents.") }),
        &["id"],
    )
}
