//! Per-route checks on the inference plane (api-docs design §7.1): Anthropic
//! routes carry the `anthropic-version` prefill, `/v1/models/{id}` is a
//! wildcard param, `/tokenize` is tagged `llamacpp` and POST only. WP6.

use lmgw_api_types::openapi_ext as ext;
use lmgw_core::openapi::admin_doc;

/// One `paths.<path>.<method>` operation object.
fn op(doc: &serde_json::Value, method: &str, path: &str) -> serde_json::Value {
    doc["paths"][path][method].clone()
}

fn params(op: &serde_json::Value) -> &[serde_json::Value] {
    op["parameters"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

// ---------------------------------------------------------------------------
// Anthropic routes carry the anthropic-version prefill (WP7 gap (b)).
// ---------------------------------------------------------------------------

#[test]
fn anthropic_routes_carry_the_anthropic_version_prefill() {
    let doc = admin_doc();
    for (method, path) in [
        ("post", "/v1/messages"),
        ("post", "/v1/messages/count_tokens"),
    ] {
        let operation = op(doc, method, path);
        assert_eq!(
            operation[ext::DIALECT],
            "anthropic",
            "{method} {path} must be the anthropic dialect"
        );
        let header = params(&operation)
            .iter()
            .find(|p| p["name"] == "anthropic-version")
            .unwrap_or_else(|| panic!("{method} {path} carries no anthropic-version param"));
        assert_eq!(header[ext::PREFILL], true, "{header:#?}");
        assert_eq!(header["in"], "header");
    }

    // A non-Anthropic route must not carry it.
    let chat = op(doc, "post", "/v1/chat/completions");
    assert!(
        !params(&chat)
            .iter()
            .any(|p| p["name"] == "anthropic-version"),
        "{chat:#?}"
    );
}

// ---------------------------------------------------------------------------
// /mcp* routes carry the Accept prefill (WP7 gap (b)).
// ---------------------------------------------------------------------------

#[test]
fn mcp_routes_carry_the_accept_prefill() {
    let doc = admin_doc();
    for (method, path) in [
        ("post", "/mcp"),
        ("get", "/mcp"),
        ("delete", "/mcp"),
        ("post", "/mcp/admin"),
        ("get", "/mcp/admin"),
        ("delete", "/mcp/admin"),
    ] {
        let operation = op(doc, method, path);
        let header = params(&operation)
            .iter()
            .find(|p| p["name"] == "Accept")
            .unwrap_or_else(|| panic!("{method} {path} carries no Accept param"));
        assert_eq!(header[ext::PREFILL], true, "{header:#?}");
    }
    let chat = op(doc, "post", "/v1/chat/completions");
    assert!(
        !params(&chat).iter().any(|p| p["name"] == "Accept"),
        "{chat:#?}"
    );
}

// ---------------------------------------------------------------------------
// /v1/models/{id} is a wildcard param.
// ---------------------------------------------------------------------------

#[test]
fn models_by_id_param_is_a_wildcard() {
    let doc = admin_doc();
    let operation = op(doc, "get", "/v1/models/{id}");
    let id_param = params(&operation)
        .iter()
        .find(|p| p["name"] == "id")
        .expect("no id path param");
    assert_eq!(id_param["in"], "path");
    assert_eq!(id_param[ext::WILDCARD], true, "{id_param:#?}");
}

// ---------------------------------------------------------------------------
// /tokenize is tagged llamacpp.
// ---------------------------------------------------------------------------

#[test]
fn tokenize_is_tagged_llamacpp() {
    let doc = admin_doc();
    let operation = op(doc, "post", "/tokenize");
    assert_eq!(operation["tags"], serde_json::json!(["llamacpp"]));
    assert_eq!(operation[ext::DIALECT], "llamacpp");
}

/// `/tokenize` is POST only, as llama.cpp's own is: the document lists no
/// other method, and the router answers any other with `405` naming `POST`
/// — not a `404` that would read as "no such route", and not a count.
#[tokio::test]
async fn get_tokenize_is_405() {
    let doc = admin_doc();
    let methods: Vec<&String> = doc["paths"]["/tokenize"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(methods, ["post"]);

    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let gw = crate::common::serve(state).await;
    let resp = gw
        .client()
        .get(format!("{gw}/tokenize"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 405);
    let allow = resp
        .headers()
        .get("allow")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(allow.contains("POST"), "Allow: {allow:?}");
}

// ---------------------------------------------------------------------------
// Every inference-plane operation with a JSON request body carries its
// hand-written example beside the schema (§4.10, WP7 gap (a)), and every
// example's top-level `model` (when the schema has one) is the tester's own
// placeholder.
// ---------------------------------------------------------------------------

#[test]
fn post_routes_with_a_model_field_carry_the_placeholder_example() {
    let doc = admin_doc();
    for (method, path) in [
        ("post", "/v1/chat/completions"),
        ("post", "/v1/messages"),
        ("post", "/v1/messages/count_tokens"),
        ("post", "/v1/completions"),
        ("post", "/v1/embeddings"),
        ("post", "/v1/rerank"),
        ("post", "/v1/count_tokens"),
        ("post", "/tokenize"),
        ("post", "/v1/images/generations"),
        ("post", "/v1/responses"),
    ] {
        let operation = op(doc, method, path);
        let schema = &operation["requestBody"]["content"]["application/json"]["schema"];
        let example = schema
            .get("example")
            .unwrap_or_else(|| panic!("{method} {path} has no requestBody example: {schema:#?}"));
        assert_eq!(
            example["model"], "<pick a model>",
            "{method} {path}: {example:#?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Every operation carries a `default` response, SSE-documented ones
// included (WP7 gap 4 — `Resp::Sse` used to return before adding one).
// ---------------------------------------------------------------------------

#[test]
fn every_inference_operation_has_a_default_response() {
    let doc = admin_doc();
    let paths = doc["paths"].as_object().unwrap();
    let mut checked = 0;
    for (path, methods) in paths {
        for (method, operation) in methods.as_object().unwrap() {
            if operation[ext::CAPABILITY] != "inference" {
                continue;
            }
            assert!(
                operation["responses"].get("default").is_some(),
                "{method} {path} has no default response"
            );
            checked += 1;
        }
    }
    assert!(checked >= 25, "only checked {checked} operations");
}

// ---------------------------------------------------------------------------
// ImageEditsRequest.image is a file array, not a bare, type-less field (WP7
// gap 3 — it used to render as a text input).
// ---------------------------------------------------------------------------

#[test]
fn image_edits_image_field_is_an_array_of_binary_files() {
    let doc = admin_doc();
    let image = &doc["components"]["schemas"]["ImageEditsRequest"]["properties"]["image"];
    assert_eq!(image["type"], "array", "{image:#?}");
    assert_eq!(image["items"]["type"], "string", "{image:#?}");
    assert_eq!(image["items"]["format"], "binary", "{image:#?}");
}

// ---------------------------------------------------------------------------
// GET /v1/openapi.json — the developer document: no admin operation
// (§4.5), served live.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v1_openapi_json_is_served_as_the_developer_document() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let gw = crate::common::serve(state).await;

    let resp = gw
        .client()
        .get(format!("{gw}/v1/openapi.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["paths"]["/v1/chat/completions"]["post"].is_object());
    assert!(body["paths"].get("/api/openapi.json").is_none());
    for (_, methods) in body["paths"].as_object().unwrap() {
        for (_, operation) in methods.as_object().unwrap() {
            assert_ne!(operation[ext::CAPABILITY], "admin");
        }
    }
    assert!(body["paths"]["/chat/api/feed"]["get"].is_object());
    assert!(body["paths"]["/api/agents/runs/{job_id}/events"]["post"].is_object());
    assert!(body["paths"]["/api/version"]["get"].is_object());
    assert!(body["paths"].get("/api/session").is_none());
}
