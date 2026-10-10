//! OpenAPI extension-key names and their fixed value vocabularies (api-docs
//! design §4.4), shared verbatim between the document `lmgw-core::openapi`
//! builds and the page that reads it back (`lmgw-ui::pages::api_ref`) — so a
//! key typoed on one side is a compile error on the other, not a rail item
//! that silently never finds its capability chip.
//!
//! Not feature-gated: the UI reads these at runtime and never generates a
//! schema, so `schemars` (and the DTOs' `schema` feature) has nothing to do
//! with this module.

// -- Extension keys, one per row of the §4.4 table ---------------------------

/// Operation: the route's or op's [`crate::openapi_ext`] capability, `Cap::as_str()`.
pub const CAPABILITY: &str = "x-lmgw-capability";
/// Operation: whether it changes stored state (§4.2).
pub const WRITES: &str = "x-lmgw-writes";
/// Operation: `true` where the answer carries a plaintext credential —
/// `key_create`, `key_reveal`, `key_rotate`, `agent_token_get`,
/// `agent_token_rotate`.
pub const REVEALS_SECRET: &str = "x-lmgw-reveals-secret";
/// Operation: an extra sentence the confirm click shows.
pub const CONFIRM_NOTE: &str = "x-lmgw-confirm-note";
/// Op operation: the op name.
pub const OP: &str = "x-lmgw-op";
/// Op operation: the op this one is a deprecated alias of.
pub const ALIAS_OF: &str = "x-lmgw-alias-of";
/// Operation: the `lmgw__X` self-admin tool sharing this code path.
pub const TOOL: &str = "x-lmgw-tool";
/// Operation: `openai` | `anthropic` | `llamacpp` | `jsonrpc` | `dashboard`.
pub const DIALECT: &str = "x-lmgw-dialect";
/// Operation: `chat` | `embedding` | `rerank` | `tts` | `asr` |
/// `image_generation` | `image_edit`.
pub const MODEL_TASK: &str = "x-lmgw-model-task";
/// Operation: the `lmgw.endpoints` groups this route belongs to.
pub const ENDPOINTS: &str = "x-lmgw-endpoints";
/// Response: why no schema exists.
pub const UNTYPED: &str = "x-lmgw-untyped";
/// Response: `{event name: schema ref}` for an SSE stream.
pub const SSE_EVENTS: &str = "x-lmgw-sse-events";
/// Path param: `true` when a `{*rest}` capture, sent unescaped.
pub const WILDCARD: &str = "x-lmgw-wildcard";
/// Property or param: `true` where the value is a credential — every
/// property or parameter named `token`, `api_key`, `hf_token` or
/// `update_token`, plus the fields marked at their definition with
/// [`secret`] (`extra_headers`, `forge_tokens`, an MCP server's `env` and
/// `headers`, an agent's config `values`).
pub const SECRET: &str = "x-lmgw-secret";
/// Header param: `true` when the tester should fill in the example value.
pub const PREFILL: &str = "x-lmgw-prefill";
/// Header param: `client` | `agent` | `owner`.
pub const AUDIENCE: &str = "x-lmgw-audience";
/// Tag: the tag's group name.
pub const GROUP: &str = "x-lmgw-group";
/// Root: `{owner, agent, key, device, anonymous, anonymous_auth_off: [caps]}`.
pub const PRINCIPALS: &str = "x-lmgw-principals";
/// Root: `[{method, path, reason}]`, the coverage exclusion list (§4.2).
pub const UNDOCUMENTED: &str = "x-lmgw-undocumented";

/// A field's schema transform that flags it [`SECRET`]:
/// `#[schemars(transform = lmgw_api_types::openapi_ext::secret)]`.
///
/// For the fields whose *name* says nothing — `env`, `headers`, `values` are
/// ordinary words elsewhere — but whose value is a credential here, often an
/// object or list of them (review R2 #3). The name-based rule in
/// `lmgw-core`'s `openapi/build.rs` covers the names that are a credential
/// wherever they appear. A transform rather than `extend("x-lmgw-secret" =
/// true)` so the key is written once, here, and not as a string literal
/// beside every field.
#[cfg(feature = "schema")]
pub fn secret(schema: &mut schemars::Schema) {
    schema.insert(SECRET.to_string(), true.into());
}

/// A field's schema transform that drops `null` from its types:
/// `#[schemars(transform = lmgw_api_types::openapi_ext::non_null)]`.
///
/// For an `Option` field that is skipped when `None` (`skip_serializing_if`)
/// and so is absent or has a value, never `null`; the derive documents every
/// `Option` as nullable.
#[cfg(feature = "schema")]
pub fn non_null(schema: &mut schemars::Schema) {
    if let Some(object) = schema.as_object_mut() {
        drop_null(object);
    }
}

/// [`non_null`] on a schema that is already a plain JSON object (the document
/// builder drops `null` from every field the gateway leaves out rather than
/// sends as `null`).
#[cfg(feature = "schema")]
pub fn drop_null(schema: &mut serde_json::Map<String, serde_json::Value>) {
    use serde_json::Value;
    if let Some(Value::Array(types)) = schema.get_mut("type") {
        types.retain(|t| t != "null");
        if types.len() == 1 {
            let only = types.remove(0);
            schema.insert("type".to_string(), only);
        }
    }
    for key in ["anyOf", "oneOf"] {
        let Some(Value::Array(arms)) = schema.get_mut(key) else {
            continue;
        };
        arms.retain(|a| a.get("type").and_then(Value::as_str) != Some("null"));
        if arms.len() == 1 {
            let Value::Object(only) = arms.remove(0) else {
                continue;
            };
            schema.remove(key);
            for (k, v) in only {
                schema.insert(k, v);
            }
        }
    }
}

/// A struct's schema transform that lists every property as `required`:
/// `#[schemars(transform = lmgw_api_types::openapi_ext::require_all)]`.
///
/// For an answer type whose container-level `#[serde(default)]` (a lenient
/// reader) would otherwise leave the schema with no `required` at all,
/// though the gateway always sends every field.
#[cfg(feature = "schema")]
pub fn require_all(schema: &mut schemars::Schema) {
    let names: Vec<serde_json::Value> = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|p| p.keys().map(|k| k.clone().into()).collect())
        .unwrap_or_default();
    if !names.is_empty() {
        schema.insert("required".to_string(), names.into());
    }
}

// -- Value vocabularies -------------------------------------------------------

/// [`DIALECT`] values.
pub mod dialect {
    pub const OPENAI: &str = "openai";
    pub const ANTHROPIC: &str = "anthropic";
    pub const LLAMACPP: &str = "llamacpp";
    pub const JSONRPC: &str = "jsonrpc";
    pub const DASHBOARD: &str = "dashboard";
    /// The Audio lab's own routes: the dashboard's, but a failure is `{error, code?}`.
    pub const LAB: &str = "lab";
}

/// [`MODEL_TASK`] values.
pub mod model_task {
    pub const CHAT: &str = "chat";
    pub const EMBEDDING: &str = "embedding";
    pub const RERANK: &str = "rerank";
    pub const TTS: &str = "tts";
    pub const ASR: &str = "asr";
    pub const IMAGE_GENERATION: &str = "image_generation";
    pub const IMAGE_EDIT: &str = "image_edit";
}

/// [`AUDIENCE`] values.
pub mod audience {
    pub const CLIENT: &str = "client";
    pub const AGENT: &str = "agent";
    pub const OWNER: &str = "owner";
}

/// [`ENDPOINTS`] group names (`/v1/models` `lmgw.endpoints`, §4.11).
pub mod endpoint_group {
    pub const OPENAI: &str = "openai";
    pub const ANTHROPIC: &str = "anthropic";
    pub const OTHER: &str = "other";
}
