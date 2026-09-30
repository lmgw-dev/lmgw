//! Whole-document guards over both served descriptions: the shape a schema
//! lands in, every `$ref` resolving, a size ceiling, and what the two routes
//! put on the wire (review R2 #1); the secret-flagged inputs, per operation
//! (R2 #3); OpenAPI 3.1's own structural rules (R3 #2).
//!
//! The admin document once weighed 16 MB: schemars' `root_schema_for` copies
//! every definition built so far into each root it returns, and 125 roots
//! each carried a copy of the whole `components.schemas`. These tests are
//! what keeps that from coming back unnoticed — the documents still validated
//! and rendered then, they were only enormous.

use crate::common;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use lmgw_api_types::openapi_ext as ext;
use lmgw_core::openapi::{admin_doc, v1_doc};
use lmgw_core::state::AppState;

fn both() -> [(&'static str, &'static Value); 2] {
    [("admin", admin_doc()), ("v1", v1_doc())]
}

// ---------------------------------------------------------------------------
// Every schema registered once
// ---------------------------------------------------------------------------

/// Every JSON pointer below `value` at which an object has a key named in
/// `keys` — except inside a `properties` map, where a key is a property
/// *name* (a DTO field called `components` would be legitimate there).
fn keys_below(value: &Value, keys: &[&str], at: &str, in_properties: bool, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let here = format!("{at}/{}", k.replace('~', "~0").replace('/', "~1"));
                if !in_properties && keys.contains(&k.as_str()) {
                    out.push(here.clone());
                }
                keys_below(v, keys, &here, !in_properties && k == "properties", out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                keys_below(v, keys, &format!("{at}/{i}"), false, out);
            }
        }
        _ => {}
    }
}

#[test]
fn no_schema_carries_its_own_definitions() {
    for (name, doc) in both() {
        let mut found = Vec::new();
        keys_below(
            &doc["paths"],
            &["components", "$defs"],
            "/paths",
            false,
            &mut found,
        );
        // The root `components.schemas` holds every definition exactly once;
        // a schema *inside* it carrying another copy is the same bug.
        keys_below(
            &doc["components"]["schemas"],
            &["components", "$defs"],
            "/components/schemas",
            true,
            &mut found,
        );
        assert!(
            found.is_empty(),
            "{name}: nested definition copies at {found:#?} — a SchemaFn result \
             reached the document without going through schemas::embed"
        );
    }
}

/// Every `$ref` value in `value`, with the pointer it sits at.
fn refs(value: &Value, at: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let here = format!("{at}/{}", k.replace('~', "~0").replace('/', "~1"));
                if k == "$ref" {
                    if let Some(target) = v.as_str() {
                        out.push((here.clone(), target.to_string()));
                    }
                }
                refs(v, &here, out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                refs(v, &format!("{at}/{i}"), out);
            }
        }
        _ => {}
    }
}

#[test]
fn every_ref_resolves_against_the_document_root() {
    for (name, doc) in both() {
        let mut all = Vec::new();
        refs(doc, "", &mut all);
        assert!(
            all.len() > 50,
            "{name}: the ref walk found only {}",
            all.len()
        );
        let dangling: Vec<_> = all
            .iter()
            .filter(|(_, target)| {
                // A document-local pointer, and not `#` itself: schemars
                // writes `"$ref": "#"` for a type that refers to itself from
                // inside a *standalone* root, which here would mean the whole
                // OpenAPI document.
                !target.starts_with("#/") || doc.pointer(&target[1..]).is_none()
            })
            .collect();
        assert!(dangling.is_empty(), "{name}: dangling $refs {dangling:#?}");
    }
}

#[test]
fn both_documents_stay_small() {
    // Generous on purpose: the point is catching a regression of the 16 MB
    // kind (R2 #1: 16.1 MB before, ~0.56 MB after), not policing growth.
    const ADMIN_MAX: usize = 2_000_000;
    const V1_MAX: usize = 1_000_000;
    let admin = serde_json::to_vec(admin_doc()).unwrap().len();
    let v1 = serde_json::to_vec(v1_doc()).unwrap().len();
    assert!(admin < ADMIN_MAX, "admin document is {admin} bytes");
    assert!(v1 < V1_MAX, "v1 document is {v1} bytes");
}

// ---------------------------------------------------------------------------
// On the wire: cached bytes, a validator, compression
// ---------------------------------------------------------------------------

#[tokio::test]
async fn served_documents_carry_a_validator_and_compress() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state).await;
    let client = gw.client();

    for (url, doc) in [
        (format!("{gw}/api/openapi.json"), admin_doc()),
        (format!("{gw}/v1/openapi.json"), v1_doc()),
    ] {
        let first = client.get(&url).send().await.unwrap();
        assert_eq!(first.status(), 200, "{url}");
        let headers = first.headers().clone();
        assert_eq!(
            headers.get("content-type").unwrap(),
            "application/json",
            "{url}"
        );
        assert_eq!(headers.get("cache-control").unwrap(), "no-cache", "{url}");
        let etag = headers
            .get("etag")
            .unwrap_or_else(|| panic!("{url}: no ETag"))
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            headers.get("content-encoding").is_none(),
            "{url}: compressed without being asked"
        );
        let body = first.bytes().await.unwrap();
        assert_eq!(
            body.as_ref(),
            serde_json::to_vec(doc).unwrap().as_slice(),
            "{url}: the served bytes are the in-process document"
        );

        let revalidated = client
            .get(&url)
            .header("if-none-match", &etag)
            .send()
            .await
            .unwrap();
        assert_eq!(revalidated.status(), 304, "{url}");
        assert_eq!(revalidated.headers().get("etag").unwrap(), etag.as_str());
        assert!(revalidated.bytes().await.unwrap().is_empty());

        let stale = client
            .get(&url)
            .header("if-none-match", "W/\"not-this-build\"")
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), 200, "{url}");

        let gzipped = client
            .get(&url)
            .header("accept-encoding", "gzip")
            .send()
            .await
            .unwrap();
        assert_eq!(gzipped.status(), 200, "{url}");
        assert_eq!(
            gzipped.headers().get("content-encoding").unwrap(),
            "gzip",
            "{url}"
        );
        let compressed = gzipped.bytes().await.unwrap();
        assert!(
            compressed.len() < body.len() / 3,
            "{url}: gzip gave {} of {} bytes",
            compressed.len(),
            body.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Secret inputs, per operation (review R2 #3)
// ---------------------------------------------------------------------------

/// Every input of `op` flagged `x-lmgw-secret`: `in:name` for a parameter,
/// `body.<dotted.path>` for a request-body property at any depth, `$ref`s
/// followed (each component once per walk).
fn secret_inputs(doc: &Value, op: &Value) -> BTreeSet<String> {
    fn walk(
        doc: &Value,
        schema: &Value,
        path: &str,
        seen: &mut BTreeSet<String>,
        out: &mut BTreeSet<String>,
    ) {
        if let Some(target) = schema.get("$ref").and_then(Value::as_str) {
            if seen.insert(target.to_string()) {
                if let Some(resolved) = doc.pointer(target.trim_start_matches('#')) {
                    walk(doc, resolved, path, seen, out);
                }
            }
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (name, prop) in props {
                let here = format!("{path}.{name}");
                if prop.get(ext::SECRET) == Some(&Value::Bool(true)) {
                    out.insert(here.clone());
                }
                walk(doc, prop, &here, seen, out);
            }
        }
        for key in ["items", "additionalProperties"] {
            if let Some(child) = schema.get(key).filter(|c| c.is_object()) {
                walk(doc, child, path, seen, out);
            }
        }
        for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
            for child in schema
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                walk(doc, child, path, seen, out);
            }
        }
    }

    let mut out = BTreeSet::new();
    for p in op["parameters"].as_array().into_iter().flatten() {
        if p.get(ext::SECRET) == Some(&Value::Bool(true)) {
            out.insert(format!(
                "{}:{}",
                p["in"].as_str().unwrap(),
                p["name"].as_str().unwrap()
            ));
        }
    }
    for media in op
        .pointer("/requestBody/content")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|c| c.values())
    {
        walk(
            doc,
            &media["schema"],
            "body",
            &mut BTreeSet::new(),
            &mut out,
        );
    }
    out
}

/// The whole set, pinned: a secret that loses its flag (a renamed field, a
/// hand schema rewritten without it) fails here, and so does a new input
/// flagged by accident. The API reference's "Copy as curl" prints exactly
/// these as `<secret: name>` placeholders, whole values included — an object
/// or a list of header pairs is one placeholder, not a partly-redacted
/// structure.
#[test]
fn secret_inputs_per_operation() {
    let expected: BTreeMap<&str, &[&str]> = BTreeMap::from([
        ("get /api/session/login", &["query:token"][..]),
        ("post /api/session", &["body.token"][..]),
        ("post /api/op/upstream_set", &["body.api_key"][..]),
        (
            "post /api/op/upstream_set_full",
            &["body.api_key", "body.extra_headers"][..],
        ),
        (
            "post /api/op/settings_set_full",
            &["body.forge_tokens", "body.hf_token", "body.update_token"][..],
        ),
        (
            "post /api/op/mcp_server_set",
            &["body.env", "body.headers"][..],
        ),
        ("post /api/op/agent_config_set", &["body.values"][..]),
        ("post /api/op/agent_open_chat", &["body.values"][..]),
        ("post /api/op/agent_run", &["body.values"][..]),
    ]);

    let doc = admin_doc();
    let mut actual: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (path, methods) in doc["paths"].as_object().unwrap() {
        for (method, op) in methods.as_object().unwrap() {
            let found = secret_inputs(doc, op);
            if !found.is_empty() {
                actual.insert(format!("{method} {path}"), found);
            }
        }
    }
    let expected: BTreeMap<String, BTreeSet<String>> = expected
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
        .collect();
    assert_eq!(actual, expected);

    // The response side of the same credentials: an MCP server row answers
    // its `env` and `headers` as stored.
    let view = &doc["components"]["schemas"]["McpServerView"]["properties"];
    assert_eq!(view["env"][ext::SECRET], true);
    assert_eq!(view["headers"][ext::SECRET], true);
}

// ---------------------------------------------------------------------------
// OpenAPI 3.1's own structural rules (review R3 #2)
// ---------------------------------------------------------------------------
//
// A hand check rather than a validator crate: the published 3.1 meta-schema
// would have to be vendored and fed to `jsonschema`, and what broke here (a
// Response Object with no `description`) is exactly the kind of rule a short
// list states plainly. What each list below allows is the 3.1 spec's own
// field table for that object, plus `x-*` extensions everywhere.

fn allowed(key: &str, fields: &[&str]) -> bool {
    key.starts_with("x-") || fields.contains(&key)
}

const METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// A JSON Schema's structural walk: every subschema position, never an
/// `example` or `default` value (which can legitimately hold a `type` key —
/// a chat content part's `"type": "text"`).
fn check_schema(s: &Value, at: &str, errs: &mut Vec<String>) {
    const TYPES: &[&str] = &[
        "null", "boolean", "object", "array", "number", "string", "integer",
    ];
    let Some(map) = s.as_object() else {
        if !s.is_boolean() {
            errs.push(format!("{at}: a schema must be an object or a boolean"));
        }
        return;
    };
    match map.get("type") {
        None => {}
        Some(Value::String(t)) if TYPES.contains(&t.as_str()) => {}
        Some(Value::Array(ts))
            if ts
                .iter()
                .all(|t| t.as_str().is_some_and(|t| TYPES.contains(&t))) => {}
        Some(other) => errs.push(format!("{at}: bad type {other}")),
    }
    if let Some(r) = map.get("required") {
        if !r.as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
            errs.push(format!("{at}: required must be an array of strings"));
        }
    }
    for key in ["properties", "patternProperties"] {
        if let Some(props) = map.get(key) {
            match props.as_object() {
                Some(props) => {
                    for (name, sub) in props {
                        check_schema(sub, &format!("{at}/{key}/{name}"), errs);
                    }
                }
                None => errs.push(format!("{at}/{key}: must be an object")),
            }
        }
    }
    for key in ["items", "additionalProperties", "not", "contains"] {
        if let Some(sub) = map.get(key) {
            check_schema(sub, &format!("{at}/{key}"), errs);
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(list) = map.get(key) {
            match list.as_array() {
                Some(list) if !list.is_empty() => {
                    for (i, sub) in list.iter().enumerate() {
                        check_schema(sub, &format!("{at}/{key}/{i}"), errs);
                    }
                }
                _ => errs.push(format!("{at}/{key}: must be a non-empty array")),
            }
        }
    }
}

fn check_media(content: &Value, at: &str, errs: &mut Vec<String>) {
    let Some(content) = content.as_object().filter(|c| !c.is_empty()) else {
        errs.push(format!("{at}: content must be a non-empty object"));
        return;
    };
    for (mime, media) in content {
        let here = format!("{at}/{mime}");
        for key in media.as_object().into_iter().flat_map(|m| m.keys()) {
            if !allowed(key, &["schema", "example", "examples", "encoding"]) {
                errs.push(format!("{here}: unknown Media Type field {key}"));
            }
        }
        if let Some(schema) = media.get("schema") {
            check_schema(schema, &format!("{here}/schema"), errs);
        }
    }
}

fn check_operation(
    doc: &Value,
    path: &str,
    method: &str,
    op: &Value,
    ids: &mut BTreeSet<String>,
    errs: &mut Vec<String>,
) {
    let at = format!("{method} {path}");
    let Some(op) = op.as_object() else {
        errs.push(format!("{at}: not an object"));
        return;
    };
    for key in op.keys() {
        if !allowed(
            key,
            &[
                "tags",
                "summary",
                "description",
                "externalDocs",
                "operationId",
                "parameters",
                "requestBody",
                "responses",
                "callbacks",
                "deprecated",
                "security",
                "servers",
            ],
        ) {
            errs.push(format!("{at}: unknown Operation field {key}"));
        }
    }
    match op.get("operationId").and_then(Value::as_str) {
        Some(id) if !ids.insert(id.to_string()) => {
            errs.push(format!("{at}: duplicate operationId {id}"))
        }
        Some(_) => {}
        None => errs.push(format!("{at}: no operationId")),
    }

    // Tags are strings the root declares.
    let declared: BTreeSet<&str> = doc["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for tag in op
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if !tag.as_str().is_some_and(|t| declared.contains(t)) {
            errs.push(format!("{at}: tag {tag} is not declared at the root"));
        }
    }

    // Parameters: name + in, path ones required and matching the template.
    let template: BTreeSet<String> = path
        .split('/')
        .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
        .map(String::from)
        .collect();
    let mut seen = BTreeSet::new();
    let mut path_params = BTreeSet::new();
    for p in op
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = p["name"].as_str().unwrap_or_default();
        let location = p["in"].as_str().unwrap_or_default();
        if name.is_empty() || !["query", "header", "path", "cookie"].contains(&location) {
            errs.push(format!("{at}: parameter {p} needs a name and a valid `in`"));
            continue;
        }
        let key = if location == "header" {
            format!("{location}:{}", name.to_ascii_lowercase())
        } else {
            format!("{location}:{name}")
        };
        if !seen.insert(key.clone()) {
            errs.push(format!("{at}: duplicate parameter {key}"));
        }
        if p.get("schema").is_none() && p.get("content").is_none() {
            errs.push(format!(
                "{at}: parameter {key} has neither schema nor content"
            ));
        }
        if let Some(schema) = p.get("schema") {
            check_schema(schema, &format!("{at} parameter {key}"), errs);
        }
        if location == "path" {
            if p["required"] != true {
                errs.push(format!("{at}: path parameter {name} must be required"));
            }
            path_params.insert(name.to_string());
        }
    }
    if template != path_params {
        errs.push(format!(
            "{at}: template names {template:?} but path parameters are {path_params:?}"
        ));
    }

    if let Some(body) = op.get("requestBody") {
        check_media(&body["content"], &format!("{at} requestBody"), errs);
        if body.get("required").is_some_and(|r| !r.is_boolean()) {
            errs.push(format!("{at}: requestBody.required must be a boolean"));
        }
    }

    // Responses: at least one; every key a status or `default`; every
    // Response Object described.
    let Some(responses) = op
        .get("responses")
        .and_then(Value::as_object)
        .filter(|r| !r.is_empty())
    else {
        errs.push(format!("{at}: no responses"));
        return;
    };
    for (status, response) in responses {
        let here = format!("{at} response {status}");
        let status_ok = status == "default"
            || (status.len() == 3
                && status.as_bytes()[0].is_ascii_digit()
                && (status[1..].bytes().all(|b| b.is_ascii_digit()) || &status[1..] == "XX"));
        if !status_ok {
            errs.push(format!("{here}: not a status code"));
        }
        if response.get("$ref").is_some() {
            continue;
        }
        if !response["description"].is_string() {
            errs.push(format!("{here}: no description"));
        }
        for key in response.as_object().into_iter().flat_map(|m| m.keys()) {
            if !allowed(key, &["description", "headers", "content", "links"]) {
                errs.push(format!("{here}: unknown Response field {key}"));
            }
        }
        if let Some(content) = response.get("content") {
            check_media(content, &here, errs);
        }
        for (name, header) in response
            .get("headers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if header.get("name").is_some() || header.get("in").is_some() {
                errs.push(format!("{here}: header {name} must not carry name/in"));
            }
            if header.get("schema").is_none() && header.get("content").is_none() {
                errs.push(format!(
                    "{here}: header {name} has neither schema nor content"
                ));
            }
        }
    }

    // Security: every requirement names declared schemes.
    for req in op
        .get("security")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for (scheme, scopes) in req.as_object().into_iter().flatten() {
            if doc["components"]["securitySchemes"].get(scheme).is_none() {
                errs.push(format!("{at}: security names undeclared scheme {scheme}"));
            }
            if !scopes.is_array() {
                errs.push(format!(
                    "{at}: security scopes for {scheme} must be an array"
                ));
            }
        }
    }
}

fn structural_errors(doc: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    if doc["openapi"] != "3.1.0" {
        errs.push(format!("openapi is {}", doc["openapi"]));
    }
    if !doc["info"]["title"].is_string() || !doc["info"]["version"].is_string() {
        errs.push("info needs a title and a version".into());
    }
    for key in doc.as_object().unwrap().keys() {
        if !allowed(
            key,
            &[
                "openapi",
                "info",
                "jsonSchemaDialect",
                "servers",
                "paths",
                "webhooks",
                "components",
                "security",
                "tags",
                "externalDocs",
            ],
        ) {
            errs.push(format!("unknown root field {key}"));
        }
    }
    let mut tag_names = BTreeSet::new();
    for tag in doc["tags"].as_array().into_iter().flatten() {
        match tag["name"].as_str() {
            Some(n) if !tag_names.insert(n) => errs.push(format!("duplicate tag {n}")),
            Some(_) => {}
            None => errs.push(format!("tag without a name: {tag}")),
        }
    }
    for key in doc["components"]
        .as_object()
        .into_iter()
        .flat_map(|c| c.keys())
    {
        if !allowed(
            key,
            &[
                "schemas",
                "responses",
                "parameters",
                "examples",
                "requestBodies",
                "headers",
                "securitySchemes",
                "links",
                "callbacks",
                "pathItems",
            ],
        ) {
            errs.push(format!("unknown components field {key}"));
        }
    }
    for (name, scheme) in doc["components"]["securitySchemes"]
        .as_object()
        .into_iter()
        .flatten()
    {
        let ok = match scheme["type"].as_str() {
            Some("apiKey") => {
                scheme["name"].is_string()
                    && ["query", "header", "cookie"].contains(&scheme["in"].as_str().unwrap_or(""))
            }
            Some("http") => scheme["scheme"].is_string(),
            Some("mutualTLS" | "oauth2" | "openIdConnect") => true,
            _ => false,
        };
        if !ok {
            errs.push(format!("security scheme {name} is malformed: {scheme}"));
        }
    }
    for (name, schema) in doc["components"]["schemas"]
        .as_object()
        .into_iter()
        .flatten()
    {
        check_schema(schema, &format!("#/components/schemas/{name}"), &mut errs);
    }

    let mut ids = BTreeSet::new();
    let Some(paths) = doc["paths"].as_object() else {
        errs.push("paths is not an object".into());
        return errs;
    };
    for (path, item) in paths {
        if !path.starts_with('/') {
            errs.push(format!("path {path} does not start with /"));
        }
        for (key, op) in item.as_object().into_iter().flatten() {
            if METHODS.contains(&key.as_str()) {
                check_operation(doc, path, key, op, &mut ids, &mut errs);
            } else if !allowed(
                key,
                &["$ref", "summary", "description", "servers", "parameters"],
            ) {
                errs.push(format!("{path}: unknown Path Item field {key}"));
            }
        }
    }
    errs
}

#[test]
fn both_documents_are_structurally_valid_openapi_3_1() {
    for (name, doc) in both() {
        let errs = structural_errors(doc);
        assert!(
            errs.is_empty(),
            "{name}: {} problems:\n{}",
            errs.len(),
            errs.join("\n")
        );
    }
}

#[test]
fn the_structural_check_catches_what_it_claims_to() {
    // The concrete bug this check was written for, plus a few neighbours —
    // so a check that silently stopped looking fails here, not never.
    let broken = serde_json::json!({
        "openapi": "3.1.0",
        "info": {"title": "t", "version": "1"},
        "tags": [{"name": "a"}],
        "paths": {
            "/x/{id}": {"get": {
                "operationId": "x",
                "tags": ["a", "undeclared"],
                "responses": {"200": {"content": {"text/event-stream": {"schema": {"type": "string"}}}}},
            }},
            "/y": {"post": {"operationId": "x", "responses": {}}},
        },
        "components": {"schemas": {"S": {"type": "strnig"}}},
    });
    let errs = structural_errors(&broken).join("\n");
    for expected in [
        "get /x/{id} response 200: no description",
        "tag \"undeclared\" is not declared",
        "template names {\"id\"} but path parameters are {}",
        "post /y: duplicate operationId x",
        "post /y: no responses",
        "#/components/schemas/S: bad type",
    ] {
        assert!(errs.contains(expected), "missing {expected:?} in:\n{errs}");
    }
}
