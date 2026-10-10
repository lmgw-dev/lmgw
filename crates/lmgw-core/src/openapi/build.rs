//! Assembly (api-docs design §4.3, §4.11): the two served documents, built
//! once behind an [`OnceLock`] each — everything they read is static, so
//! there is nothing to invalidate — from the pieces this module's siblings
//! hold: the route registry, the op table, the tag and header constants, and
//! `server::CAPABILITY_TABLE`, which is where every operation's capability
//! comes from (never repeated in a `DocRoute` or an `OpDoc`).
//!
//! WP1 built this over an empty registry and an empty op table; `planes/*`
//! (WP4, WP6) and `ops/table.rs` (WP5) filled in without changing this
//! module's shape — a route or an op simply started showing up.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde_json::{json, Map, Value};

use lmgw_api_types::openapi_ext;

use crate::config::{ApiKeyKind, Snapshot};
use crate::principal::{Cap, Principal};
use crate::server::CAPABILITY_TABLE;

use super::exclusions::UNDOCUMENTED;
use super::headers::Direction;
use super::ops::{self, OpArgs, OpDoc};
use super::params;
use super::registry::{self, Dialect, DocRoute, Req, Resp};
use super::schemas;
use super::tags::TAGS;
use super::v1;

static ADMIN_DOC: OnceLock<Value> = OnceLock::new();
static V1_DOC: OnceLock<Value> = OnceLock::new();

/// Everything (§1 "Served twice"): every plane, every op, gated only by the
/// caller's own capability at request time.
///
/// A reference, not a clone: the document is built once and never changes,
/// and `serve.rs` serializes it once too — a caller that wants its own copy
/// to edit says so with `.clone()` (review R2 #1: the handlers used to clone
/// the whole tree per request).
pub fn admin_doc() -> &'static Value {
    ADMIN_DOC.get_or_init(build_admin)
}

/// The developer document: the admin document filtered to the operations
/// [`is_developer_op`] selects (inference, the device Chat API, the agent run
/// API, `GET /api/version`), then pruned (§4.5) — its paths, its schemas, and
/// the root keys that only describe the admin plane (`prune_v1_root`).
pub fn v1_doc() -> &'static Value {
    V1_DOC.get_or_init(|| {
        let mut doc = admin_doc().clone();
        filter_to_developer(&mut doc);
        schemas::prune_components(&mut doc);
        prune_v1_root(&mut doc);
        doc
    })
}

// ---------------------------------------------------------------------------
// The admin document
// ---------------------------------------------------------------------------

fn build_admin() -> Value {
    let mut g = schemas::generator();
    let mut sent = schemas::Sent::new();
    let mut paths: Map<String, Value> = Map::new();

    for route in registry::all_routes() {
        let cap = capability_for(route.method, route.path);
        let op_path = params::openapi_path(route.path);
        let entry = paths
            .entry(op_path)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("a paths entry is always an object");
        entry.insert(
            route.method.to_ascii_lowercase(),
            route_operation(&mut g, &mut sent, &route, cap),
        );
    }

    let op_cap = capability_for("POST", "/api/op/{name}");
    for op in ops::table() {
        paths.insert(
            format!("/api/op/{}", op.name),
            json!({ "post": op_operation(&mut g, &mut sent, &op, op_cap) }),
        );
    }

    // An answer's `required` lists say what the server always sends, not what
    // its tolerant reader would accept (see `apply_sent_required`).
    let reads = request_reach(&paths, &g);
    schemas::apply_sent_required(&mut g, sent, &reads, &mut paths);

    let components = json!({
        "schemas": Value::Object(g.definitions().clone()),
        "securitySchemes": security_schemes(),
    });

    let mut doc = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "lmgw",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "lmgw's own HTTP API: the inference plane (OpenAI-, \
                Anthropic- and llama.cpp-compatible, plus MCP), the device (Chat) \
                API paired clients use, the agent run API agents use, and the \
                dashboard that configures the gateway.",
        },
        "servers": [{ "url": "/" }],
        "tags": tags_json(),
        "paths": paths,
        "components": components,
        (openapi_ext::PRINCIPALS): principals_matrix(),
        (openapi_ext::UNDOCUMENTED): undocumented_json(),
    });
    mark_secrets(&mut doc);
    doc
}

/// The components a request body or a parameter reaches, `$ref`s followed:
/// their `required` lists stay the reader's (`schemas::apply_sent_required`).
fn request_reach(paths: &Map<String, Value>, g: &schemars::SchemaGenerator) -> BTreeSet<String> {
    let mut frontier: Vec<String> = Vec::new();
    for methods in paths.values().filter_map(Value::as_object) {
        for op in methods.values() {
            for key in ["requestBody", "parameters"] {
                if let Some(v) = op.get(key) {
                    schemas::collect_refs(v, &mut frontier);
                }
            }
        }
    }
    let mut seen = BTreeSet::new();
    while let Some(name) = frontier.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(schema) = g.definitions().get(&name) {
            schemas::collect_refs(schema, &mut frontier);
        }
    }
    seen
}

/// Property and parameter names that are a credential wherever they appear
/// (§4.4 `x-lmgw-secret`): a request-body property named one of these, or a
/// parameter named one of these, is flagged `x-lmgw-secret: true` wherever it
/// appears, `$ref`-reached components included (walking the whole assembled
/// document, not `g.definitions()` before it is copied into `components`, is
/// what reaches those).
///
/// Half of the rule (review R2 #3). A name like `env`, `headers` or `values`
/// is a credential in one struct and an ordinary word in the next, so those
/// fields are flagged where they are defined instead — the
/// `lmgw_api_types::openapi_ext::secret` schemars transform on a struct
/// field, `x-lmgw-secret` written into a hand-written schema (`ops/args.rs`).
/// `openapi_structure.rs`'s `secret_inputs_per_operation` pins the result.
const SECRET_NAMES: &[&str] = &["token", "api_key", "hf_token", "update_token"];

/// Walk the whole document once, marking every `properties.<name>` schema and
/// every parameter object whose `name` is in [`SECRET_NAMES`].
fn mark_secrets(value: &mut Value) {
    if let Value::Object(map) = value {
        if let Some(Value::Object(props)) = map.get_mut("properties") {
            for name in SECRET_NAMES {
                if let Some(Value::Object(prop)) = props.get_mut(*name) {
                    prop.insert(openapi_ext::SECRET.to_string(), json!(true));
                }
            }
        }
        // A parameter object: `{"name": ..., "in": ...}` (query/header/path,
        // never a schema property — those have no `"in"`).
        let is_secret_param = matches!(
            (map.get("name"), map.get("in")),
            (Some(Value::String(name)), Some(_)) if SECRET_NAMES.contains(&name.as_str())
        );
        if is_secret_param {
            map.insert(openapi_ext::SECRET.to_string(), json!(true));
        }
        for v in map.values_mut() {
            mark_secrets(v);
        }
    } else if let Value::Array(items) = value {
        for v in items {
            mark_secrets(v);
        }
    }
}

/// The row `route.method`/`route.path` names, read from the one table that
/// says so (§4.1) — a `DocRoute` for a row that table does not have is a bug
/// this panic catches at doc-build time rather than at `route_walk`'s.
fn capability_for(method: &str, path: &str) -> Cap {
    CAPABILITY_TABLE
        .iter()
        .find(|(m, p, _)| *m == method && *p == path)
        .map(|(_, _, cap)| *cap)
        .unwrap_or_else(|| {
            panic!(
                "openapi: {method} {path} is not a CAPABILITY_TABLE row — capability is read \
                 from that table, never repeated on a DocRoute (api-docs design §4.1)"
            )
        })
}

fn tags_json() -> Value {
    Value::Array(
        TAGS.iter()
            .map(|t| {
                let description = if t.description.is_empty() {
                    t.name
                } else {
                    t.description
                };
                json!({
                    "name": t.id,
                    "description": description,
                    // The ReDoc-style short label (WP7 gap 2, UI review): `description` is the long sentence a tag with
                    // one (e.g. `agent-runtime`) carries, which is the wrong
                    // thing for a rail header to print in full. `name` is
                    // always the short one.
                    "x-displayName": t.name,
                    (openapi_ext::GROUP): t.group.as_str(),
                })
            })
            .collect(),
    )
}

fn security_schemes() -> Value {
    json!({
        "bearer": {
            "type": "http",
            "scheme": "bearer",
            "description": "An lmgw API key of any kind. A paired device's key is refused \
                with 401 device_key_unknown once it matches no device (the device was rotated \
                or deleted: pair it again), 401 device_disabled while the device is disabled \
                (the same key works once it is enabled again), and 401 key_expired past its \
                expiry date. A retry with the same key does not help with any of the three.",
        },
        "apiKeyHeader": {
            "type": "apiKey",
            "in": "header",
            "name": "x-api-key",
            "description": "The same keys, in the header Anthropic SDKs send them in.",
        },
        "session": {
            "type": "apiKey",
            "in": "cookie",
            "name": crate::principal::SESSION_COOKIE,
            "description": "The dashboard's own session cookie. Same-origin only, and only \
                ever an owner row.",
        },
        "adminToken": {
            "type": "apiKey",
            "in": "header",
            "name": "x-lmgw-admin-token",
            "description": "POST /mcp/admin only.",
        },
    })
}

fn security_for(cap: Cap, method: &str, path: &str) -> Value {
    let mut schemes: Vec<Value> = match cap {
        Cap::Public => vec![json!({})],
        Cap::Inference => vec![
            json!({"bearer": []}),
            json!({"apiKeyHeader": []}),
            json!({"session": []}),
            json!({}),
        ],
        Cap::Admin | Cap::AgentSelf | Cap::Chat => vec![
            json!({"bearer": []}),
            json!({"apiKeyHeader": []}),
            json!({"session": []}),
        ],
        Cap::Ledger => vec![json!({"bearer": []}), json!({"apiKeyHeader": []})],
    };
    if method == "POST" && path == "/mcp/admin" {
        schemes.push(json!({"adminToken": []}));
    }
    Value::Array(schemes)
}

// ---------------------------------------------------------------------------
// Route operations
// ---------------------------------------------------------------------------

fn operation_id(method: &str, openapi_path: &str) -> String {
    let slug: String = openapi_path
        .chars()
        .map(|c| match c {
            '/' | '{' | '}' | '-' | '.' => '_',
            other => other,
        })
        .collect();
    let collapsed = slug
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    format!("{}_{collapsed}", method.to_ascii_lowercase())
}

fn route_operation(
    g: &mut schemars::SchemaGenerator,
    sent: &mut schemas::Sent,
    route: &DocRoute,
    cap: Cap,
) -> Value {
    let openapi_path = params::openapi_path(route.path);
    let is_inference = cap == Cap::Inference;

    let mut parameters = params::path_params(route.path, route.path_ints);
    if let Some(query) = route.query {
        parameters.extend(params::query_params(g, query));
    }
    parameters.extend(params::header_params(
        route.method,
        route.path,
        is_inference,
        Direction::Request,
    ));
    parameters.extend(prefill_header_params(route));

    let mut obj = Map::new();
    obj.insert(
        "operationId".into(),
        json!(operation_id(route.method, &openapi_path)),
    );
    obj.insert("summary".into(), json!(route.summary));
    if !route.description.is_empty() {
        obj.insert("description".into(), json!(route.description));
    }
    obj.insert("tags".into(), json!([route.tag]));
    if !parameters.is_empty() {
        obj.insert("parameters".into(), Value::Array(parameters));
    }
    if let Some(body) = request_body(g, route.request, route.example.map(|f| f())) {
        obj.insert("requestBody".into(), body);
    }
    obj.insert(
        "responses".into(),
        responses_object(
            g,
            route.response,
            route.dialect,
            route.method,
            route.path,
            is_inference,
            sent,
        ),
    );
    obj.insert(
        "security".into(),
        security_for(cap, route.method, route.path),
    );
    obj.insert(openapi_ext::CAPABILITY.into(), json!(cap.as_str()));
    obj.insert(
        openapi_ext::WRITES.into(),
        json!(route.writes.unwrap_or(route.method != "GET")),
    );
    obj.insert(openapi_ext::DIALECT.into(), json!(route.dialect.as_str()));
    if !route.endpoints.is_empty() {
        obj.insert(openapi_ext::ENDPOINTS.into(), json!(route.endpoints));
    }
    if let Some(tool) = route.tool {
        obj.insert(openapi_ext::TOOL.into(), json!(tool));
    }
    if let Some(task) = route.model_task {
        obj.insert(openapi_ext::MODEL_TASK.into(), json!(task));
    }
    if let Some(note) = route.confirm_note {
        obj.insert(openapi_ext::CONFIRM_NOTE.into(), json!(note));
    }
    Value::Object(obj)
}

/// `x-lmgw-prefill` header parameters (§4.4): headers the tester fills in the
/// example value for, on the two families that need one to work at all —
/// `anthropic-version` on the Anthropic routes (`/v1/messages`,
/// `/v1/messages/count_tokens`) and `Accept` on `/mcp*` (the Streamable HTTP
/// MUST-list requires it name both `application/json` and
/// `text/event-stream`, `mcp/ingress.rs:13`).
///
/// **WP7 gap (b), fixed here.** Neither header is `x-lmgw-*`, so neither
/// lives in [`super::headers::LMGW_HEADERS`] — this is deliberately the one
/// place that decides which operations get the flag, rather than growing the
/// shared header table with two entries that are not lmgw's own headers.
fn prefill_header_params(route: &DocRoute) -> Vec<Value> {
    let mut out = Vec::new();
    if route.dialect == Dialect::Anthropic {
        out.push(json!({
            "name": "anthropic-version",
            "in": "header",
            "required": false,
            "description": "The Anthropic SDK's protocol version header. lmgw does not \
                validate its value or vary its behaviour on it; egress to a real Anthropic \
                upstream sends its own.",
            "schema": { "type": "string" },
            "example": "2023-06-01",
            (openapi_ext::PREFILL): true,
        }));
    }
    if route.path == "/mcp" || route.path == "/mcp/admin" {
        out.push(json!({
            "name": "Accept",
            "in": "header",
            "required": false,
            "description": "MCP Streamable HTTP requires the client accept both JSON and \
                server-sent events.",
            "schema": { "type": "string" },
            "example": "application/json, text/event-stream",
            (openapi_ext::PREFILL): true,
        }));
    }
    out
}

/// `example`, when given, lands beside the schema value's own `$ref` (or,
/// for a hand-written inline schema, beside its other keys) — not on the
/// media-type wrapper. `lmgw-ui`'s `example.rs::body_example` checks
/// `schema.get("example")` *before* following any `$ref`, precisely so an
/// example can sit next to a `$ref` without needing a dedicated per-component
/// example slot; matching that shape is the point (§4.10; WP7 gap (a) — a
/// `DocRoute`/`OpDoc` example used to reach nowhere in the built document, so
/// the tester never saw it).
fn request_body(
    g: &mut schemars::SchemaGenerator,
    req: Req,
    example: Option<Value>,
) -> Option<Value> {
    match req {
        Req::None => None,
        Req::Json(schema_fn) => {
            let mut schema = schemas::embed(g, schema_fn);
            if let Some(ex) = example {
                schema["example"] = ex;
            }
            Some(json!({
                "required": true,
                "content": { "application/json": { "schema": schema } },
            }))
        }
        Req::OptionalJson(schema_fn) => {
            let mut schema = schemas::embed(g, schema_fn);
            if let Some(ex) = example {
                schema["example"] = ex;
            }
            Some(json!({
                "required": false,
                "content": { "application/json": { "schema": schema } },
            }))
        }
        Req::Multipart(schema_fn) => {
            let mut schema = schemas::embed(g, schema_fn);
            if let Some(ex) = example {
                schema["example"] = ex;
            }
            Some(json!({
                "required": true,
                "content": { "multipart/form-data": { "schema": schema } },
            }))
        }
        Req::JsonOrNdjson(schema_fn) => {
            let mut schema = schemas::embed(g, schema_fn);
            if let Some(ex) = example {
                schema["example"] = ex;
            }
            Some(json!({
                "required": true,
                "content": {
                    "application/json": { "schema": schema },
                    "application/x-ndjson": { "schema": {
                        "type": "string",
                        "description": "Newline-delimited JSON: one value per line, each \
                            decoded as the application/json form's single-object case.",
                    } },
                },
            }))
        }
        Req::Raw(mime) => Some(json!({
            "required": true,
            "content": { mime: { "schema": { "type": "string", "format": "binary" } } },
        })),
        Req::JsonRpc => {
            // WP1 bug, fixed in WP6: this used to `schemas::named_ref` a
            // component named "JsonRpcRequest" that nothing ever registered
            // (the same dangling-ref shape as `schemas::error_ref`, see its
            // doc comment) — `v1::mcp::jsonrpc_request` both registers it and
            // returns the ref, on the same `g` every other schema here uses.
            let mut schema = v1::mcp::jsonrpc_request(g).to_value();
            if let Some(ex) = example {
                schema["example"] = ex;
            }
            Some(json!({
                "required": true,
                "content": { "application/json": { "schema": schema } },
            }))
        }
    }
}

fn responses_object(
    g: &mut schemars::SchemaGenerator,
    resp: Resp,
    dialect: Dialect,
    method: &str,
    path: &str,
    is_inference: bool,
    sent: &mut schemas::Sent,
) -> Value {
    let mut responses = Map::new();
    let (status, body) = match resp {
        Resp::Json(schema_fn) => {
            let schema = sent.embed(g, schema_fn);
            (
                "200",
                Some(json!({ "application/json": { "schema": schema } })),
            )
        }
        // WP1 bug, fixed in WP5: `named_ref` alone builds a `$ref` string
        // with nothing registered behind it (the same dangling-ref shape
        // `schemas::error_ref`'s own doc comment describes) — `OPS` was
        // still empty in WP1, so nothing exercised this arm. `subschema_for`
        // is what `v1::errors::api_error` already uses for a plain
        // `JsonSchema`-derived api-types struct: it registers the schema
        // under the type's own name and returns the `$ref` in one call.
        Resp::OpOutcome => {
            sent.touch(|g| g.subschema_for::<lmgw_api_types::OpOutcome>());
            (
                "200",
                Some(json!({
                    "application/json": {
                        "schema": g.subschema_for::<lmgw_api_types::OpOutcome>().to_value(),
                    },
                })),
            )
        }
        Resp::Untyped(why) => (
            "200",
            Some(json!({ "application/json": { "schema": schemas::untyped(why) } })),
        ),
        Resp::Binary(mimes) => {
            let content: Map<String, Value> = mimes
                .iter()
                .map(|m| {
                    (
                        (*m).to_string(),
                        json!({ "schema": { "type": "string", "format": "binary" } }),
                    )
                })
                .collect();
            ("200", Some(Value::Object(content)))
        }
        Resp::Download(media) => {
            let content: Map<String, Value> = media
                .iter()
                .map(|(mime, schema_fn)| {
                    let schema = match schema_fn {
                        Some(f) => sent.embed(g, *f),
                        None => json!({ "type": "string", "format": "binary" }),
                    };
                    ((*mime).to_string(), json!({ "schema": schema }))
                })
                .collect();
            ("200", Some(Value::Object(content)))
        }
        Resp::Sse(events) => {
            let mut sse_events = Map::new();
            for (name, schema_fn) in events {
                sse_events.insert((*name).to_string(), sent.embed(g, *schema_fn));
            }
            let mut media = Map::new();
            media.insert(
                "text/event-stream".to_string(),
                json!({ "schema": { "type": "string" } }),
            );
            // `description` is required on every Response Object (OpenAPI
            // 3.1 §4.8.17); this arm alone had none, which made both
            // documents invalid — GET /mcp is in the v1 one (review R3 #2).
            let mut response = json!({ "description": "An event stream.", "content": media });
            response[openapi_ext::SSE_EVENTS] = Value::Object(sse_events);
            responses.insert(
                "200".to_string(),
                with_headers(response, method, path, is_inference),
            );
            // WP7 gap 4 (UI review): this returned here, so an SSE
            // operation carried no `default` response at all — every other
            // arm gives one (§4.3 "the documented success status ... A
            // `default` response with the dialect's error schema"), and an
            // SSE route can fail before the stream even opens (a bad body, a
            // refused key) exactly like a JSON one.
            responses.insert(
                "default".to_string(),
                json!({
                    "description": "Error.",
                    "content": { "application/json": { "schema": schemas::error_ref(g, dialect) } },
                }),
            );
            return Value::Object(responses);
        }
        Resp::JsonOrSse {
            json: schema_fn,
            events,
        } => {
            let json_schema = sent.embed(g, schema_fn);
            let mut sse_events = Map::new();
            for (name, event_fn) in events {
                sse_events.insert((*name).to_string(), sent.embed(g, *event_fn));
            }
            let mut media = Map::new();
            media.insert(
                "application/json".to_string(),
                json!({ "schema": json_schema }),
            );
            media.insert(
                "text/event-stream".to_string(),
                json!({ "schema": { "type": "string" } }),
            );
            let mut response = json!({ "description": "OK.", "content": media });
            response[openapi_ext::SSE_EVENTS] = Value::Object(sse_events);
            responses.insert(
                "200".to_string(),
                with_headers(response, method, path, is_inference),
            );
            responses.insert(
                "default".to_string(),
                json!({
                    "description": "Error.",
                    "content": { "application/json": { "schema": schemas::error_ref(g, dialect) } },
                }),
            );
            return Value::Object(responses);
        }
        Resp::WebSocket {
            subprotocol,
            frames,
        } => {
            let mut switching = json!({
                "description": format!(
                    "Switching Protocols: the connection is now a WebSocket. {frames}"
                ),
            });
            // A route with no subprotocol of its own (the device host link)
            // selects none.
            if !subprotocol.is_empty() {
                switching["headers"] = json!({
                    "Sec-WebSocket-Protocol": {
                        "description": "The subprotocol selected, present when the client \
                            offered it.",
                        "schema": { "type": "string", "enum": [subprotocol] },
                    },
                });
            }
            responses.insert(
                "101".to_string(),
                with_headers(switching, method, path, is_inference),
            );
            responses.insert(
                "426".to_string(),
                json!({
                    "description": "Upgrade Required: the request carried no WebSocket \
                        upgrade headers.",
                    "content": { "application/json": { "schema": schemas::error_ref(g, dialect) } },
                }),
            );
            responses.insert(
                "default".to_string(),
                json!({
                    "description": "Error, before the upgrade.",
                    "content": { "application/json": { "schema": schemas::error_ref(g, dialect) } },
                }),
            );
            return Value::Object(responses);
        }
        Resp::Redirect => {
            responses.insert(
                "302".to_string(),
                with_headers(
                    json!({ "description": "Redirect." }),
                    method,
                    path,
                    is_inference,
                ),
            );
            responses.insert(
                "default".to_string(),
                json!({
                    "description": "Error.",
                    "content": { "application/json": { "schema": schemas::error_ref(g, dialect) } },
                }),
            );
            return Value::Object(responses);
        }
        Resp::NoContent => {
            responses.insert(
                "204".to_string(),
                with_headers(
                    json!({ "description": "No content." }),
                    method,
                    path,
                    is_inference,
                ),
            );
            responses.insert(
                "default".to_string(),
                json!({
                    "description": "Error.",
                    "content": { "application/json": { "schema": schemas::error_ref(g, dialect) } },
                }),
            );
            return Value::Object(responses);
        }
        Resp::Doc => (
            "200",
            Some(json!({ "application/json": { "schema": { "type": "object" } } })),
        ),
    };
    let mut response = json!({ "description": "OK." });
    if let Some(content) = body {
        response["content"] = content;
    }
    responses.insert(
        status.to_string(),
        with_headers(response, method, path, is_inference),
    );
    responses.insert(
        "default".to_string(),
        json!({
            "description": "Error.",
            "content": { "application/json": { "schema": schemas::error_ref(g, dialect) } },
        }),
    );
    Value::Object(responses)
}

/// Response headers from the header table (§4.3 "Response headers from §4.9
/// on every non-default response").
fn with_headers(mut response: Value, method: &str, path: &str, is_inference: bool) -> Value {
    let headers = params::header_params(method, path, is_inference, Direction::Response);
    if headers.is_empty() {
        return response;
    }
    // Added to whatever headers the response already declares (a WebSocket
    // 101's `Sec-WebSocket-Protocol`), never in place of them.
    let mut obj = response["headers"].as_object().cloned().unwrap_or_default();
    for h in headers {
        let name = h["name"].as_str().unwrap_or_default().to_string();
        obj.insert(
            name,
            json!({ "description": h["description"], "schema": h["schema"] }),
        );
    }
    response["headers"] = Value::Object(obj);
    response
}

// ---------------------------------------------------------------------------
// Op operations (§4.7)
// ---------------------------------------------------------------------------

fn op_operation(
    g: &mut schemars::SchemaGenerator,
    sent: &mut schemas::Sent,
    op: &OpDoc,
    cap: Cap,
) -> Value {
    let mut obj = Map::new();
    obj.insert("operationId".into(), json!(format!("op_{}", op.name)));
    obj.insert("summary".into(), json!(op.summary));
    if let Some(description) = ops::description(op.description, op.tool) {
        obj.insert("description".into(), json!(description));
    }
    obj.insert("tags".into(), json!([op.tag]));
    if let Some(body) = op_request_body(g, op) {
        obj.insert("requestBody".into(), body);
    }
    obj.insert(
        "responses".into(),
        responses_object(
            g,
            op.response,
            Dialect::Dashboard,
            "POST",
            "/api/op/{name}",
            false,
            sent,
        ),
    );
    obj.insert(
        "security".into(),
        security_for(cap, "POST", "/api/op/{name}"),
    );
    obj.insert(openapi_ext::CAPABILITY.into(), json!(cap.as_str()));
    obj.insert(openapi_ext::WRITES.into(), json!(op.writes));
    obj.insert(openapi_ext::OP.into(), json!(op.name));
    if op.reveals_secret {
        obj.insert(openapi_ext::REVEALS_SECRET.into(), json!(true));
    }
    if let Some(tool) = op.tool {
        obj.insert(openapi_ext::TOOL.into(), json!(tool));
    }
    if let Some(note) = op.confirm_note {
        obj.insert(openapi_ext::CONFIRM_NOTE.into(), json!(note));
    }
    if op.deprecated {
        obj.insert("deprecated".into(), json!(true));
    }
    if let OpArgs::AliasOf(target) = op.args {
        obj.insert(openapi_ext::ALIAS_OF.into(), json!(target));
    }
    Value::Object(obj)
}

/// `example`, when the op table gives one, is JSON text rather than a `fn` —
/// `OpDoc::example` is meant to be readable sitting next to the table row
/// (§4.10 "an explicit JSON example in the table"), unlike a `DocRoute`'s,
/// which has no such row to sit in. It lands on the schema the same way a
/// route's does (WP7 gap (a)); a literal that fails to parse is a table bug,
/// caught here at doc-build time rather than shipped to the tester broken.
///
/// **WP1 bug, fixed in WP5.** `OpArgs::Hand`'s function returns the argument
/// *schema* itself (table D: "a worker … wrote the schema by hand"), not an
/// example — this used to ignore it and emit a bare `{"type":"object"}`
/// (nothing called this arm while `OPS` was still empty). `OpArgs::Tool` used
/// to answer `None` — no `requestBody` at all — for table A's "same tool
/// args" rows; the tool's own `inputSchema` (`ops::tool_schema`) is what
/// those ops actually take. And a `Struct` op sharing a tool now gets that
/// tool's per-property prose overlaid (`ops::overlay_tool_props`, §4.7's
/// merge rule) rather than only ever showing the struct's own doc comments.
fn op_request_body(g: &mut schemars::SchemaGenerator, op: &OpDoc) -> Option<Value> {
    let mut schema = match op.args {
        OpArgs::Struct(schema_fn) => {
            let mut schema = schemas::embed(g, schema_fn);
            if let Some(tool) = op.tool {
                ops::overlay_tool_props(&mut schema, tool);
            }
            schema
        }
        OpArgs::Tool => {
            let tool = op.tool.unwrap_or_else(|| {
                panic!(
                    "openapi: op '{}' is OpArgs::Tool but names no tool",
                    op.name
                )
            });
            ops::tool_schema(tool)
        }
        OpArgs::Hand(schema_fn) => schema_fn(),
        OpArgs::NoArgs | OpArgs::AliasOf(_) => return None,
    };
    if let Some(text) = op.example {
        schema["example"] = serde_json::from_str::<Value>(text)
            .unwrap_or_else(|e| panic!("openapi: op example is not valid JSON ({e}): {text}"));
    }
    Some(json!({
        "required": true,
        "content": { "application/json": { "schema": schema } },
    }))
}

// ---------------------------------------------------------------------------
// Principals + undocumented
// ---------------------------------------------------------------------------

/// The capabilities the developer document's operations use: every one but
/// `admin`, which is [`is_developer_op`]'s rule.
const DEVELOPER_CAPS: [Cap; 5] = [
    Cap::Public,
    Cap::Inference,
    Cap::Chat,
    Cap::Ledger,
    Cap::AgentSelf,
];

const CAPS: [Cap; 6] = [
    Cap::Public,
    Cap::Inference,
    Cap::Chat,
    Cap::Ledger,
    Cap::AgentSelf,
    Cap::Admin,
];

fn holds_list(principal: &Principal, snap: &Snapshot) -> Vec<&'static str> {
    CAPS.iter()
        .filter(|cap| principal.holds(**cap, snap))
        .map(|cap| cap.as_str())
        .collect()
}

/// `x-lmgw-principals` (§4.4): what each of the six principal kinds holds,
/// computed at build time from [`Principal::holds`] on synthetic principals —
/// this document cannot itself drift from what the gate actually decides.
fn principals_matrix() -> Value {
    principals_matrix_for(&CAPS)
}

/// [`principals_matrix`] limited to the capabilities `covered` lists: the
/// developer document keeps the key-kind map for the capabilities its own
/// operations use (everything but `admin`).
fn principals_matrix_for(covered: &[Cap]) -> Value {
    let held = |principal: &Principal, snap: &Snapshot| -> Vec<&'static str> {
        holds_list(principal, snap)
            .into_iter()
            .filter(|name| covered.iter().any(|cap| cap.as_str() == *name))
            .collect()
    };
    let mut auth_on = Snapshot::default();
    auth_on.settings.auth_enabled = true;
    let mut auth_off = Snapshot::default();
    auth_off.settings.auth_enabled = false;

    let owner = Principal::Key {
        id: 0,
        name: String::new(),
        kind: ApiKeyKind::Owner,
        agent_id: None,
        fingerprint: String::new(),
    };
    let agent = Principal::Key {
        id: 0,
        name: String::new(),
        kind: ApiKeyKind::Agent,
        agent_id: None,
        fingerprint: String::new(),
    };
    let key = Principal::Key {
        id: 0,
        name: String::new(),
        kind: ApiKeyKind::Key,
        agent_id: None,
        fingerprint: String::new(),
    };
    let device = Principal::Key {
        id: 0,
        name: String::new(),
        kind: ApiKeyKind::Device,
        agent_id: None,
        fingerprint: String::new(),
    };
    let anonymous = Principal::Anonymous;

    json!({
        "owner": held(&owner, &auth_on),
        "agent": held(&agent, &auth_on),
        "key": held(&key, &auth_on),
        "device": held(&device, &auth_on),
        "anonymous": held(&anonymous, &auth_on),
        "anonymous_auth_off": held(&anonymous, &auth_off),
    })
}

fn undocumented_json() -> Value {
    Value::Array(
        UNDOCUMENTED
            .iter()
            .map(|(method, path, reason)| {
                json!({ "method": method, "path": path, "reason": reason })
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// The v1 filter (§4.5)
// ---------------------------------------------------------------------------

/// The v1 document's root, cut down to what its own operations use (review
/// R2 #5, R3 #5) — [`filter_to_developer`] only ever touched `paths`, so the
/// root kept describing the admin plane:
///
/// - `tags`: only the ones an operation names (the `ops-*` and the dashboard
///   tags have nothing left to group);
/// - `components.securitySchemes`: only the ones an operation's `security`
///   lists (`adminToken` is `POST /mcp/admin`'s alone);
/// - `x-lmgw-principals` stays, restricted to the capabilities the document
///   covers (no `admin`): who may call what is part of what a developer reads;
/// - `x-lmgw-undocumented` goes: it is an inventory of the admin plane's own
///   exclusions, dashboard mini-APIs included, which stays in the admin
///   document only (spec §12).
fn prune_v1_root(doc: &mut Value) {
    let mut tags: BTreeSet<String> = BTreeSet::new();
    let mut schemes: BTreeSet<String> = BTreeSet::new();
    if let Some(paths) = doc.get("paths").and_then(Value::as_object) {
        for op in paths
            .values()
            .filter_map(Value::as_object)
            .flat_map(|m| m.values())
        {
            for tag in op["tags"].as_array().into_iter().flatten() {
                if let Some(t) = tag.as_str() {
                    tags.insert(t.to_string());
                }
            }
            for entry in op["security"].as_array().into_iter().flatten() {
                if let Some(names) = entry.as_object() {
                    schemes.extend(names.keys().cloned());
                }
            }
        }
    }
    if let Some(list) = doc.get_mut("tags").and_then(Value::as_array_mut) {
        list.retain(|t| t["name"].as_str().is_some_and(|n| tags.contains(n)));
    }
    if let Some(defined) = doc
        .pointer_mut("/components/securitySchemes")
        .and_then(Value::as_object_mut)
    {
        defined.retain(|name, _| schemes.contains(name));
    }
    if let Some(root) = doc.as_object_mut() {
        root.insert(
            openapi_ext::PRINCIPALS.to_string(),
            principals_matrix_for(&DEVELOPER_CAPS),
        );
        root.remove(openapi_ext::UNDOCUMENTED);
    }
    doc["info"]["description"] = json!(
        "What a developer builds on lmgw: the inference plane (OpenAI-, Anthropic- and \
         llama.cpp-compatible routes, plus MCP), the device (Chat) API a paired client \
         uses, and the run API an agent uses with its own token. The owner's admin \
         plane, the dashboard that configures the gateway, is in the admin document, \
         GET /api/openapi.json."
    );
}

/// The public-capability operations the developer document keeps; every
/// other public operation is the dashboard's browser-session login
/// (`/api/session*`). The one list [`is_developer_op`] and the
/// classification test both read, so a new public route has to be put on one
/// side or the test fails.
const PUBLIC_DEVELOPER_OPS: &[(&str, &str)] = &[("get", "/api/version")];

/// Which operations belong in the developer document: everything intended for
/// external clients and agents building on lmgw, i.e. every operation a
/// non-owner credential reaches (capability inference, chat, agent-self,
/// ledger) plus the public ones in [`PUBLIC_DEVELOPER_OPS`] (`GET /api/version`,
/// which a client uses to check the gateway). Left out: every `admin`
/// operation (the owner's plane), the browser-session routes, and the
/// `/mcp/admin` session handlers (`GET`, `DELETE`), which only close or read
/// the session the admin-only `POST /mcp/admin` opens.
fn is_developer_op(method: &str, path: &str, op: &Value) -> bool {
    let Some(cap) = op.get(openapi_ext::CAPABILITY).and_then(Value::as_str) else {
        return false;
    };
    if path == "/mcp/admin" {
        return false;
    }
    if cap == Cap::Public.as_str() {
        return PUBLIC_DEVELOPER_OPS
            .iter()
            .any(|(m, p)| method.eq_ignore_ascii_case(m) && path == *p);
    }
    cap != Cap::Admin.as_str()
}

fn filter_to_developer(doc: &mut Value) {
    let Some(paths) = doc.get_mut("paths").and_then(Value::as_object_mut) else {
        return;
    };
    let mut empty_paths = Vec::new();
    for (path, methods) in paths.iter_mut() {
        let Some(methods_obj) = methods.as_object_mut() else {
            continue;
        };
        methods_obj.retain(|method, op| is_developer_op(method, path, op));
        if methods_obj.is_empty() {
            empty_paths.push(path.clone());
        }
    }
    for path in empty_paths {
        paths.remove(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn doc_is_structurally_sound() {
        let doc = admin_doc();
        assert_eq!(doc["openapi"], "3.1.0");

        // operationIds unique.
        let mut ids = HashSet::new();
        for methods in doc["paths"].as_object().unwrap().values() {
            for (_, op) in methods.as_object().unwrap() {
                let id = op["operationId"].as_str().unwrap();
                assert!(ids.insert(id.to_string()), "duplicate operationId {id}");
            }
        }

        // Every tag a path uses is declared.
        let declared: HashSet<&str> = TAGS.iter().map(|t| t.id).collect();
        for methods in doc["paths"].as_object().unwrap().values() {
            for (_, op) in methods.as_object().unwrap() {
                for tag in op["tags"].as_array().unwrap() {
                    let tag = tag.as_str().unwrap();
                    assert!(declared.contains(tag), "undeclared tag {tag}");
                }
            }
        }

        // Every op has a non-default response, and every $ref resolves.
        let schemas = doc["components"]["schemas"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let mut refs = Vec::new();
        for methods in doc["paths"].as_object().unwrap().values() {
            for (_, op) in methods.as_object().unwrap() {
                let responses = op["responses"].as_object().unwrap();
                assert!(
                    responses.keys().any(|k| k != "default"),
                    "{}: only a default response",
                    op["operationId"]
                );
                collect_refs_for_test(op, &mut refs);
            }
        }
        for name in &refs {
            assert!(
                schemas.contains_key(name),
                "$ref to #/components/schemas/{name} does not resolve"
            );
        }

        // Every security scheme named exists.
        let scheme_names: HashSet<&str> = doc["components"]["securitySchemes"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for methods in doc["paths"].as_object().unwrap().values() {
            for (_, op) in methods.as_object().unwrap() {
                for entry in op["security"].as_array().unwrap() {
                    for name in entry.as_object().unwrap().keys() {
                        assert!(
                            scheme_names.contains(name.as_str()),
                            "unknown scheme {name}"
                        );
                    }
                }
            }
        }
    }

    fn collect_refs_for_test(value: &Value, out: &mut Vec<String>) {
        const PREFIX: &str = "#/components/schemas/";
        match value {
            Value::String(s) => {
                if let Some(name) = s.strip_prefix(PREFIX) {
                    out.push(name.to_string());
                }
            }
            Value::Object(map) => {
                for v in map.values() {
                    collect_refs_for_test(v, out);
                }
            }
            Value::Array(items) => {
                for v in items {
                    collect_refs_for_test(v, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn principals_matrix_matches_holds() {
        let doc = admin_doc();
        let matrix = &doc[openapi_ext::PRINCIPALS];

        let mut snap = Snapshot::default();
        snap.settings.auth_enabled = true;
        let owner = Principal::Key {
            id: 0,
            name: String::new(),
            kind: ApiKeyKind::Owner,
            agent_id: None,
            fingerprint: String::new(),
        };
        assert!(owner.holds(Cap::Public, &snap));
        assert!(!owner.holds(Cap::Ledger, &snap));
        let owner_list: HashSet<&str> = matrix["owner"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(owner_list.contains("public"));
        assert!(!owner_list.contains("ledger"));
        assert!(owner_list.contains("chat"));

        // A paired device (client-apps design §1.2): the Chat API on top of
        // a client key's inference, and nothing of the admin plane.
        let device: Vec<&str> = matrix["device"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(device, ["public", "inference", "chat"]);

        let anon_off: HashSet<&str> = matrix["anonymous_auth_off"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            anon_off.contains("inference"),
            "auth off admits anonymous inference"
        );

        let anon_on: HashSet<&str> = matrix["anonymous"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            !anon_on.contains("inference"),
            "auth on refuses anonymous inference"
        );
    }

    #[test]
    fn untyped_responses_say_why() {
        let doc = admin_doc();
        for methods in doc["paths"].as_object().unwrap().values() {
            for (_, op) in methods.as_object().unwrap() {
                for (_, response) in op["responses"].as_object().unwrap() {
                    let Some(schema) = response.pointer("/content/application~1json/schema") else {
                        continue;
                    };
                    if let Some(why) = schema.get(openapi_ext::UNTYPED) {
                        assert!(
                            why.as_str().is_some_and(|s| !s.is_empty()),
                            "an untyped response must say why"
                        );
                    }
                }
            }
        }
    }

    /// Why `schema` carries no type, after following `$ref`s into `components`
    /// (`None` = typed): the `x-lmgw-untyped` marker, `true`, `{}`, an object
    /// with neither `properties` nor a typed `additionalProperties`
    /// (`additionalProperties: true` / `{}` count as free-form). `anyOf`,
    /// `oneOf`, `allOf` members and array items are looked through.
    fn free_form_reason(doc: &Value, schema: &Value, depth: usize) -> Option<&'static str> {
        if depth > 8 {
            return None;
        }
        match schema {
            Value::Bool(true) => return Some("the schema `true`"),
            Value::Object(o) if o.is_empty() => return Some("the empty schema {}"),
            Value::Object(o) => {
                if let Some(target) = o.get("$ref").and_then(Value::as_str) {
                    let resolved = doc.pointer(target.trim_start_matches('#'))?;
                    return free_form_reason(doc, resolved, depth + 1);
                }
                if o.contains_key(openapi_ext::UNTYPED) {
                    return Some("an x-lmgw-untyped marker");
                }
                for key in ["anyOf", "oneOf", "allOf"] {
                    if let Some(members) = o.get(key).and_then(Value::as_array) {
                        return members
                            .iter()
                            .find_map(|m| free_form_reason(doc, m, depth + 1));
                    }
                }
                if o.get("type").and_then(Value::as_str) == Some("array") {
                    return o
                        .get("items")
                        .and_then(|i| free_form_reason(doc, i, depth + 1));
                }
                let is_object = o.get("type").and_then(Value::as_str) == Some("object");
                let free_map = match o.get("additionalProperties") {
                    None | Some(Value::Bool(true)) => true,
                    Some(Value::Object(extra)) => extra.is_empty(),
                    Some(_) => false,
                };
                if is_object && !o.contains_key("properties") && free_map {
                    return Some("a bare free-form object");
                }
            }
            _ => {}
        }
        None
    }

    /// The typed-answers rule, enforced: the operations whose answer (every
    /// response in every content type, every `x-lmgw-sse-events` frame) or
    /// request body is untyped (marked, or free-form once `$ref`s are
    /// resolved) are exactly `exclusions::UNTYPED`.
    #[test]
    fn untyped_responses_are_exactly_the_allowlist() {
        let doc = admin_doc();
        let mut found = std::collections::BTreeSet::new();
        let mut hits = Vec::new();
        for (path, methods) in doc["paths"].as_object().unwrap() {
            for (method, op) in methods.as_object().unwrap() {
                // The documents describe themselves with a bare object: the
                // one answer whose shape is the OpenAPI specification.
                let describes_itself = path == "/api/openapi.json" || path == "/v1/openapi.json";
                let mut schemas: Vec<(String, &Value)> = Vec::new();
                for (status, response) in op["responses"].as_object().into_iter().flatten() {
                    for (mime, media) in response["content"].as_object().into_iter().flatten() {
                        if let Some(s) = media.get("schema") {
                            schemas.push((format!("{status} {mime}"), s));
                        }
                    }
                    for (event, s) in response["x-lmgw-sse-events"]
                        .as_object()
                        .into_iter()
                        .flatten()
                    {
                        schemas.push((format!("{status} event {event}"), s));
                    }
                }
                for (mime, media) in op["requestBody"]["content"]
                    .as_object()
                    .into_iter()
                    .flatten()
                {
                    if let Some(s) = media.get("schema") {
                        schemas.push((format!("request {mime}"), s));
                    }
                }
                for (what, schema) in schemas {
                    let Some(why) = free_form_reason(doc, schema, 0) else {
                        continue;
                    };
                    if describes_itself {
                        continue;
                    }
                    hits.push(format!("{method} {path} ({what}): {why}"));
                    found.insert((method.clone(), path.clone()));
                }
            }
        }
        let allowed: std::collections::BTreeSet<_> = crate::openapi::exclusions::UNTYPED
            .iter()
            .map(|(m, p, _)| (m.to_string(), p.to_string()))
            .collect();
        let new: Vec<_> = hits
            .iter()
            .filter(|h| {
                found
                    .difference(&allowed)
                    .any(|(m, p)| h.starts_with(&format!("{m} {p} ")))
            })
            .collect();
        assert!(
            new.is_empty(),
            "untyped (give each an lmgw-api-types type the handler builds or parses; only \
             verbatim relays of another program's JSON belong on exclusions::UNTYPED): {new:#?}"
        );
        if let Some((m, p)) = allowed.difference(&found).next() {
            panic!("exclusions::UNTYPED lists {m} {p}, which is no longer untyped: remove it");
        }
        assert!(
            crate::openapi::exclusions::UNTYPED
                .iter()
                .all(|(_, _, why)| !why.is_empty()),
            "an UNTYPED entry must say why"
        );
    }

    /// The public operations that are not for developers: the dashboard's login
    /// (`/api/session*`), meaningful only in a browser with the session cookie.
    fn is_browser_session_path(path: &str) -> bool {
        path == "/api/session" || path.starts_with("/api/session/")
    }

    /// Every public-capability operation is classified: a developer one (in
    /// the v1 document, on [`PUBLIC_DEVELOPER_OPS`]) or a browser-session one
    /// (out of it). A new public route is neither until someone decides.
    #[test]
    fn every_public_operation_is_developer_or_browser_session() {
        let v1 = v1_doc();
        let mut seen = 0;
        for (path, methods) in admin_doc()["paths"].as_object().unwrap() {
            for (method, op) in methods.as_object().unwrap() {
                if op[openapi_ext::CAPABILITY] != Cap::Public.as_str() {
                    continue;
                }
                seen += 1;
                let in_v1 = v1["paths"][path][method].is_object();
                if PUBLIC_DEVELOPER_OPS
                    .iter()
                    .any(|(m, p)| *m == method && p == path)
                {
                    assert!(
                        in_v1,
                        "{method} {path} is a developer operation but not in v1"
                    );
                } else {
                    assert!(
                        is_browser_session_path(path),
                        "{method} {path} is public but neither on PUBLIC_DEVELOPER_OPS nor a \
                         browser-session route: classify it in openapi/build.rs"
                    );
                    assert!(
                        !in_v1,
                        "{method} {path} is a browser-session operation in v1"
                    );
                }
            }
        }
        assert!(seen > 1, "the admin document has public operations");
    }

    #[test]
    fn named_schema_collision_panics() {
        let result = std::panic::catch_unwind(|| {
            let mut g = schemas::generator();
            schemas::named(
                &mut g,
                "Collide",
                schemars::json_schema!({"type": "string"}),
            );
            schemas::named(
                &mut g,
                "Collide",
                schemars::json_schema!({"type": "number"}),
            );
        });
        assert!(result.is_err(), "a colliding redefinition must panic");
    }

    #[test]
    fn v1_doc_is_the_developer_document_and_is_pruned() {
        // The selection rule (`is_developer_op`): no admin operation, no
        // browser-session route; the inference, device and agent planes and
        // `GET /api/version` are in.
        let v1 = v1_doc();
        let paths = v1["paths"].as_object().unwrap();
        assert!(!paths.is_empty());
        for (path, methods) in paths {
            for (method, op) in methods.as_object().unwrap() {
                assert_ne!(
                    op[openapi_ext::CAPABILITY],
                    "admin",
                    "{method} {path} is an admin operation in the v1 document"
                );
            }
        }
        assert!(paths.contains_key("/v1/chat/completions"));
        assert!(paths["/chat/api/feed"]["get"].is_object());
        assert!(paths["/api/agents/runs/{job_id}/events"]["post"].is_object());
        assert!(paths["/api/version"]["get"].is_object());
        assert!(!paths.contains_key("/api/session"));
        assert!(!paths.contains_key("/api/openapi.json"));
        // /mcp/admin is the admin document's: its POST is admin, and the
        // GET/DELETE session handlers are useless without it.
        assert!(!paths.contains_key("/mcp/admin"));
        assert!(admin_doc()["paths"]["/mcp/admin"]["get"].is_object());
        assert!(admin_doc()["paths"]["/mcp/admin"]["delete"].is_object());

        // The key-kind map is there, without the admin capability.
        let matrix = v1[openapi_ext::PRINCIPALS].as_object().unwrap();
        assert_eq!(matrix["device"], json!(["public", "inference", "chat"]));
        assert!(matrix
            .values()
            .flat_map(|v| v.as_array().unwrap())
            .all(|cap| cap != "admin"));

        // Pruning: a schema only an admin operation reaches is not in `components`.
        let schemas = v1["components"]["schemas"].as_object().unwrap();
        assert!(!schemas.is_empty());
        let admin = admin_doc()["components"]["schemas"].as_object().unwrap();
        assert!(schemas.len() < admin.len());
    }

    #[test]
    fn operation_id_matches_the_worked_examples() {
        assert_eq!(
            operation_id("POST", "/v1/chat/completions"),
            "post_v1_chat_completions"
        );
        assert_eq!(
            operation_id("GET", "/api/agents/{id}/runs"),
            "get_api_agents_id_runs"
        );
    }

    // -- WP7 (UI review) gaps 1 and 2 ------------------------------

    #[test]
    fn mark_secrets_flags_a_named_property_at_any_depth_and_a_named_param() {
        let mut doc = json!({
            "paths": {
                "/x": {"get": {"parameters": [
                    {"name": "token", "in": "query"},
                    {"name": "other", "in": "query"}
                ]}}
            },
            "components": {"schemas": {
                "Outer": {
                    "type": "object",
                    "properties": {
                        "api_key": {"type": "string"},
                        "nested": {"type": "object", "properties": {
                            "hf_token": {"type": "string"},
                            "other": {"type": "string"}
                        }}
                    }
                }
            }}
        });
        mark_secrets(&mut doc);

        assert_eq!(
            doc["paths"]["/x"]["get"]["parameters"][0][openapi_ext::SECRET],
            true
        );
        assert!(doc["paths"]["/x"]["get"]["parameters"][1]
            .get(openapi_ext::SECRET)
            .is_none());
        assert_eq!(
            doc["components"]["schemas"]["Outer"]["properties"]["api_key"][openapi_ext::SECRET],
            true
        );
        assert_eq!(
            doc["components"]["schemas"]["Outer"]["properties"]["nested"]["properties"]["hf_token"]
                [openapi_ext::SECRET],
            true
        );
        assert!(
            doc["components"]["schemas"]["Outer"]["properties"]["nested"]["properties"]["other"]
                .get(openapi_ext::SECRET)
                .is_none()
        );
    }

    #[test]
    fn secret_flags_reach_through_a_ref_in_the_built_document() {
        // The concrete worry gap 1 names: a property reached only via a
        // `$ref` into `components/schemas` must still get flagged, since
        // that is how every hand-written `/v1` schema here is stored
        // (`schemas::named` always returns a ref, never an inline body).
        let doc = admin_doc();
        let tokenize = &doc["components"]["schemas"]["TokenizeRequest"]["properties"];
        // `TokenizeRequest` carries no secret-named field itself; assert the
        // walker at least reached this $ref-only component without panicking
        // and left its ordinary fields alone.
        assert!(tokenize["model"].get(openapi_ext::SECRET).is_none());
    }

    #[test]
    fn every_tag_carries_x_display_name() {
        let doc = admin_doc();
        for tag in doc["tags"].as_array().unwrap() {
            assert_eq!(
                tag["x-displayName"],
                tag["name"]
                    .as_str()
                    .and_then(|id| TAGS.iter().find(|t| t.id == id))
                    .map(|t| t.name)
                    .unwrap(),
                "{tag:#?}"
            );
        }
        // The concrete case gap 2 names: `agent-runtime`'s description is a
        // full sentence, but its displayName is the short rail label.
        let agent_runtime = doc["tags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "agent-runtime")
            .unwrap();
        assert_eq!(agent_runtime["x-displayName"], "Agent runtime");
        assert!(agent_runtime["description"].as_str().unwrap().len() > "Agent runtime".len());
    }
}
