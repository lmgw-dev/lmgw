//! The OpenAPI document, parsed once into what the rest of `api_ref` reads
//! (api-docs design §6.4): one [`Operation`] per `paths.*.*`, the tag list in
//! document order (already group-then-tag, since `build.rs` emits `tags` in
//! `TAGS` order — see `crates/lmgw-core/src/openapi/tags.rs`), the principals
//! matrix and the coverage exclusion list. Pure — no signals, no fetch — so a
//! fixture document exercises it exactly like the page does.

use serde_json::Value;

use lmgw_api_types::openapi_ext as ext;

/// One `paths.<path>.<method>` entry, keyed by the extension constants
/// (api-docs design §4.4) rather than repeating their string literals here.
/// `parameters`/`requestBody`/`responses`/`security` stay as the raw OpenAPI
/// `value` — `detail.rs` and `tester.rs` read those directly, since remodeling
/// the whole OpenAPI operation object here would just be a second, drifting
/// copy of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Operation {
    pub operation_id: String,
    pub method: String,
    pub path: String,
    pub tag: String,
    pub tag_name: String,
    pub group: String,
    pub summary: String,
    pub description: String,
    pub capability: String,
    pub writes: bool,
    pub reveals_secret: bool,
    pub confirm_note: Option<String>,
    pub deprecated: bool,
    pub op_name: Option<String>,
    pub alias_of: Option<String>,
    pub tool: Option<String>,
    pub dialect: Option<String>,
    pub model_task: Option<String>,
    pub endpoints: Vec<String>,
    /// The full `paths.<path>.<method>` object.
    pub value: Value,
}

impl Operation {
    pub fn parameters(&self) -> &[Value] {
        self.value
            .get("parameters")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// `(mime, schema)` of the request body, if the operation has one — the
    /// first (and in practice only) content-type entry.
    pub fn request_body_schema(&self) -> Option<(&str, &Value)> {
        let content = self.value.get("requestBody")?.get("content")?.as_object()?;
        content
            .iter()
            .next()
            .map(|(mime, body)| (mime.as_str(), body.get("schema").unwrap_or(&Value::Null)))
    }

    pub fn request_body_required(&self) -> bool {
        self.value
            .get("requestBody")
            .and_then(|b| b.get("required"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn responses(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.value
            .get("responses")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|m| m.iter().map(|(k, v)| (k.as_str(), v)))
    }

    /// The first non-`default` response: the one status the operation
    /// documents a real answer for (§4.3).
    pub fn success_response(&self) -> Option<(&str, &Value)> {
        self.responses().find(|(status, _)| *status != "default")
    }

    /// Why the success response has no schema, if it does not (§4.6 rule 4).
    pub fn untyped_reason(&self) -> Option<&str> {
        let (_, resp) = self.success_response()?;
        resp.pointer("/content/application~1json/schema")?
            .get(ext::UNTYPED)?
            .as_str()
    }

    pub fn sse_events(&self) -> Option<&serde_json::Map<String, Value>> {
        let (_, resp) = self.success_response()?;
        resp.get(ext::SSE_EVENTS)?.as_object()
    }

    pub fn is_sse(&self) -> bool {
        self.success_response()
            .is_some_and(|(_, r)| r.pointer("/content/text~1event-stream").is_some())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TagInfo {
    pub id: String,
    pub name: String,
    pub group: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Principals {
    pub owner: Vec<String>,
    pub agent: Vec<String>,
    pub key: Vec<String>,
    /// A paired device (client-apps design §1.2).
    pub device: Vec<String>,
    pub anonymous: Vec<String>,
    pub anonymous_auth_off: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Undocumented {
    pub method: String,
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ApiDoc {
    pub operations: Vec<Operation>,
    /// Group-then-declaration order, exactly as `doc["tags"]` carries it.
    pub tags: Vec<TagInfo>,
    pub principals: Principals,
    pub undocumented: Vec<Undocumented>,
    /// `doc["components"]` (`schemas`, `securitySchemes`) — what
    /// [`resolve_ref`] and every schema tree read against.
    pub components: Value,
}

impl ApiDoc {
    pub fn parse(doc: &Value) -> Self {
        let tags: Vec<TagInfo> = doc
            .get("tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|t| TagInfo {
                id: str_field(t, "name"),
                // The short human label: `x-displayName` (the usual vendor
                // key for it) when the doc carries one, else the description
                // — which `build.rs` fills with the label when a tag has no
                // longer sentence of its own — else the id.
                name: [DISPLAY_NAME, "description", "name"]
                    .iter()
                    .map(|k| str_field(t, k))
                    .find(|v| !v.is_empty())
                    .unwrap_or_default(),
                group: str_field(t, ext::GROUP),
            })
            .collect();

        let mut operations = Vec::new();
        if let Some(paths) = doc.get("paths").and_then(Value::as_object) {
            for (path, methods) in paths {
                let Some(methods) = methods.as_object() else {
                    continue;
                };
                for (method, op) in methods {
                    // A path item may also carry `parameters`, `summary`…;
                    // only the HTTP verbs (and lmgw's `*` for an `any(..)`
                    // route) are operations.
                    if !is_method_key(method) {
                        continue;
                    }
                    let tag = op
                        .get("tags")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let info = tags.iter().find(|t| t.id == tag);
                    operations.push(Operation {
                        operation_id: str_field(op, "operationId"),
                        method: method.to_ascii_uppercase(),
                        path: path.clone(),
                        tag_name: info.map_or_else(|| tag.clone(), |t| t.name.clone()),
                        group: info.map(|t| t.group.clone()).unwrap_or_default(),
                        tag,
                        summary: str_field(op, "summary"),
                        description: str_field(op, "description"),
                        capability: str_field(op, ext::CAPABILITY),
                        writes: op
                            .get(ext::WRITES)
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        reveals_secret: op
                            .get(ext::REVEALS_SECRET)
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        confirm_note: opt_str_field(op, ext::CONFIRM_NOTE),
                        deprecated: op
                            .get("deprecated")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        op_name: opt_str_field(op, ext::OP),
                        alias_of: opt_str_field(op, ext::ALIAS_OF),
                        tool: opt_str_field(op, ext::TOOL),
                        dialect: opt_str_field(op, ext::DIALECT),
                        model_task: opt_str_field(op, ext::MODEL_TASK),
                        endpoints: op
                            .get(ext::ENDPOINTS)
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        value: op.clone(),
                    });
                }
            }
        }

        let principals = doc
            .get(ext::PRINCIPALS)
            .map(parse_principals)
            .unwrap_or_default();
        let undocumented = doc
            .get(ext::UNDOCUMENTED)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|u| Undocumented {
                method: str_field(u, "method"),
                path: str_field(u, "path"),
                reason: str_field(u, "reason"),
            })
            .collect();

        ApiDoc {
            operations,
            tags,
            principals,
            undocumented,
            components: doc
                .get("components")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        }
    }

    /// The rail's own order (§6.4): tag group, then tag in doc order (both
    /// already the order `self.tags` carries, since `build.rs` emits `tags`
    /// in `TAGS` order), then by path and method within a tag — which, for
    /// an op path (`/api/op/<name>`), is the same as ordering by name.
    pub fn rail_order(&self) -> Vec<&Operation> {
        let tag_rank: std::collections::HashMap<&str, usize> = self
            .tags
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id.as_str(), i))
            .collect();
        let mut ops: Vec<&Operation> = self.operations.iter().collect();
        ops.sort_by(|a, b| {
            let ra = tag_rank.get(a.tag.as_str()).copied().unwrap_or(usize::MAX);
            let rb = tag_rank.get(b.tag.as_str()).copied().unwrap_or(usize::MAX);
            ra.cmp(&rb)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| method_rank(&a.method).cmp(&method_rank(&b.method)))
        });
        ops
    }

    /// The words in a filter query all matched, over path, summary, op name
    /// and tag name (§6.4). An empty word list matches everything.
    pub fn matches(op: &Operation, words: &[String]) -> bool {
        if words.is_empty() {
            return true;
        }
        let hay = format!(
            "{} {} {} {}",
            op.path,
            op.summary,
            op.op_name.as_deref().unwrap_or(""),
            op.tag_name
        );
        words.iter().all(|w| crate::widgets::matches_word(&hay, w))
    }
}

/// A tag's short label, when a longer `description` is not it.
const DISPLAY_NAME: &str = "x-displayName";

fn is_method_key(key: &str) -> bool {
    matches!(
        key,
        "get" | "put" | "post" | "delete" | "options" | "head" | "patch" | "trace" | "*"
    )
}

/// GET first, then the writing verbs in the order a reader expects them —
/// used only to break ties within one path in [`ApiDoc::rail_order`].
fn method_rank(method: &str) -> u8 {
    match method {
        "GET" => 0,
        "POST" => 1,
        "PUT" => 2,
        "PATCH" => 3,
        "DELETE" => 4,
        _ => 5,
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn opt_str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(String::from)
}

fn parse_principals(v: &Value) -> Principals {
    let list = |k: &str| {
        v.get(k)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect()
    };
    Principals {
        owner: list("owner"),
        agent: list("agent"),
        key: list("key"),
        device: list("device"),
        anonymous: list("anonymous"),
        anonymous_auth_off: list("anonymous_auth_off"),
    }
}

/// A `{"$ref": "#/components/schemas/Name"}` resolved against `components`
/// (§6.3's `resolve_ref`) — `None` when `schema` is not a ref, or the name it
/// names does not exist.
pub fn resolve_ref<'v>(schema: &Value, components: &'v Value) -> Option<(String, &'v Value)> {
    let name = schema
        .get("$ref")?
        .as_str()?
        .strip_prefix("#/components/schemas/")?;
    let resolved = components.get("schemas")?.get(name)?;
    Some((name.to_string(), resolved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        json!({
            "tags": [
                {"name": "openai", "description": "OpenAI-compatible", (ext::GROUP): "Inference"},
                {"name": "status", "description": "Status & live feed", (ext::GROUP): "Dashboard API"},
            ],
            "paths": {
                "/v1/chat/completions": {
                    "post": {
                        "operationId": "post_v1_chat_completions",
                        "summary": "Chat completions",
                        "tags": ["openai"],
                        (ext::CAPABILITY): "inference",
                        (ext::WRITES): false,
                        "responses": {"200": {"content": {"application/json": {"schema": {"type": "object"}}}}},
                    }
                },
                "/api/status": {
                    "get": {
                        "operationId": "get_api_status",
                        "summary": "Status",
                        "tags": ["status"],
                        (ext::CAPABILITY): "public",
                        (ext::WRITES): false,
                        "responses": {"200": {"content": {"application/json": {"schema": {(ext::UNTYPED): "no DTO"}}}}},
                    }
                },
            },
            (ext::PRINCIPALS): {
                "owner": ["public", "inference", "admin"],
                "agent": ["public", "inference", "agent-self"],
                "key": ["public", "inference"],
                "anonymous": ["public"],
                "anonymous_auth_off": ["public", "inference"],
            },
            (ext::UNDOCUMENTED): [
                {"method": "GET", "path": "/", "reason": "the dashboard SPA"},
            ],
            "components": {"schemas": {"Widget": {"type": "string"}}},
        })
    }

    #[test]
    fn parses_operations_tags_and_principals() {
        let doc = ApiDoc::parse(&fixture());
        assert_eq!(doc.operations.len(), 2);
        assert_eq!(doc.tags.len(), 2);
        let chat = doc
            .operations
            .iter()
            .find(|o| o.operation_id == "post_v1_chat_completions")
            .unwrap();
        assert_eq!(chat.method, "POST");
        assert_eq!(chat.tag_name, "OpenAI-compatible");
        assert_eq!(chat.group, "Inference");
        assert!(!chat.writes);
        assert_eq!(doc.principals.owner, vec!["public", "inference", "admin"]);
        assert_eq!(doc.undocumented.len(), 1);
    }

    #[test]
    fn path_item_keys_that_are_not_methods_are_skipped() {
        let mut fx = fixture();
        fx["paths"]["/v1/chat/completions"]["parameters"] = json!([]);
        fx["paths"]["/v1/chat/completions"]["summary"] = json!("shared");
        let doc = ApiDoc::parse(&fx);
        assert_eq!(doc.operations.len(), 2);
    }

    #[test]
    fn a_display_name_wins_over_a_long_description() {
        let mut fx = fixture();
        fx["tags"][1]["description"] = json!("Every route that shows the status, at length.");
        fx["tags"][1]["x-displayName"] = json!("Status");
        let doc = ApiDoc::parse(&fx);
        assert_eq!(doc.tags[1].name, "Status");
        assert_eq!(doc.tags[0].name, "OpenAI-compatible");
    }

    #[test]
    fn untyped_reason_is_read_from_the_success_response() {
        let doc = ApiDoc::parse(&fixture());
        let status = doc
            .operations
            .iter()
            .find(|o| o.operation_id == "get_api_status")
            .unwrap();
        assert_eq!(status.untyped_reason(), Some("no DTO"));
        let chat = doc
            .operations
            .iter()
            .find(|o| o.operation_id == "post_v1_chat_completions")
            .unwrap();
        assert_eq!(chat.untyped_reason(), None);
    }

    #[test]
    fn resolve_ref_looks_up_components() {
        let doc = ApiDoc::parse(&fixture());
        let (name, schema) = resolve_ref(
            &json!({"$ref": "#/components/schemas/Widget"}),
            &doc.components,
        )
        .unwrap();
        assert_eq!(name, "Widget");
        assert_eq!(schema["type"], "string");
        assert!(resolve_ref(&json!({"type": "string"}), &doc.components).is_none());
    }

    #[test]
    fn rail_order_is_group_then_tag_then_path_and_method() {
        let mut fx = fixture();
        // A second, later-declared tag in the same group as "status", plus a
        // GET/POST pair on the same path, to prove both tie-breaks.
        fx["tags"].as_array_mut().unwrap().push(json!({
            "name": "models", "description": "Models", (ext::GROUP): "Dashboard API",
        }));
        fx["paths"]["/api/models"] = json!({
            "post": {
                "operationId": "post_api_models", "summary": "Create", "tags": ["models"],
                (ext::CAPABILITY): "admin", (ext::WRITES): true,
                "responses": {"200": {"content": {"application/json": {"schema": {"type": "object"}}}}},
            },
            "get": {
                "operationId": "get_api_models", "summary": "List", "tags": ["models"],
                (ext::CAPABILITY): "admin", (ext::WRITES): false,
                "responses": {"200": {"content": {"application/json": {"schema": {"type": "object"}}}}},
            },
        });
        let doc = ApiDoc::parse(&fx);
        let order: Vec<String> = doc
            .rail_order()
            .iter()
            .map(|o| o.operation_id.clone())
            .collect();
        // Inference group (openai) before Dashboard API (status, models).
        assert_eq!(order[0], "post_v1_chat_completions");
        // Within the models path, GET sorts before POST.
        let models_pos: Vec<usize> = order
            .iter()
            .enumerate()
            .filter(|(_, id)| id.starts_with("get_api_models") || id.starts_with("post_api_models"))
            .map(|(i, _)| i)
            .collect();
        assert!(
            order[models_pos[0]] == "get_api_models" && order[models_pos[1]] == "post_api_models"
        );
    }

    #[test]
    fn filter_matches_path_summary_op_and_tag() {
        let doc = ApiDoc::parse(&fixture());
        let chat = doc
            .operations
            .iter()
            .find(|o| o.operation_id == "post_v1_chat_completions")
            .unwrap();
        assert!(ApiDoc::matches(chat, &["chat".to_string()]));
        assert!(ApiDoc::matches(chat, &["openai".to_string()]));
        assert!(!ApiDoc::matches(chat, &["nope".to_string()]));
        assert!(ApiDoc::matches(chat, &[]));
    }
}
