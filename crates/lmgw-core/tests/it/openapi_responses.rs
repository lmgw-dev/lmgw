//! What the documents say an operation answers: error statuses with a body of
//! their own beside the dialect's `default`, and what an answer's schemas
//! carry.

use std::collections::BTreeSet;

use serde_json::Value;

use lmgw_core::openapi::{admin_doc, v1_doc};

const REF: &str = "#/components/schemas/";

/// The JSON pointers at which `value` has a `default` schema keyword: not a
/// property *called* `default`, not data inside `enum`/`const`/`example`.
fn defaults_in(value: &Value, at: &str, names: bool, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if !names && map.contains_key("default") {
                out.push(format!("{at}/default"));
            }
            for (key, child) in map {
                if !names && matches!(key.as_str(), "enum" | "const" | "example" | "examples") {
                    continue;
                }
                let props = !names && matches!(key.as_str(), "properties" | "patternProperties");
                defaults_in(child, &format!("{at}/{key}"), props, out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                defaults_in(v, &format!("{at}/{i}"), false, out);
            }
        }
        _ => {}
    }
}

fn refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(name) = map
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix(REF))
            {
                out.push(name.to_string());
            }
            map.values().for_each(|v| refs(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
        _ => {}
    }
}

/// The component names reachable from `roots`, `$ref`s followed.
fn reach(doc: &Value, roots: &[&Value]) -> BTreeSet<String> {
    let mut frontier = Vec::new();
    roots.iter().for_each(|v| refs(v, &mut frontier));
    let mut seen = BTreeSet::new();
    while let Some(name) = frontier.pop() {
        if seen.insert(name.clone()) {
            refs(&doc["components"]["schemas"][&name], &mut frontier);
        }
    }
    seen
}

/// Every operation, labelled `<verb> <path>`.
fn operations(doc: &Value) -> impl Iterator<Item = (String, &Value)> {
    doc["paths"].as_object().unwrap().iter().flat_map(|(p, m)| {
        m.as_object()
            .unwrap()
            .iter()
            .map(move |(verb, op)| (format!("{verb} {p}"), op))
    })
}

#[test]
fn an_answer_side_schema_carries_no_default_and_a_request_side_one_keeps_it() {
    for (name, doc) in [("admin", admin_doc()), ("v1", v1_doc())] {
        let mut answers: Vec<&Value> = Vec::new();
        let mut requests: Vec<&Value> = Vec::new();
        let mut found = Vec::new();
        for (label, op) in operations(doc) {
            // Each response object, not the map: its `default` key is the
            // dialect's error response.
            for (status, response) in op["responses"].as_object().unwrap() {
                defaults_in(response, &format!("{label}/{status}"), false, &mut found);
                answers.push(response);
            }
            for key in ["requestBody", "parameters"] {
                if !op[key].is_null() {
                    requests.push(&op[key]);
                }
            }
        }
        for component in reach(doc, &answers) {
            defaults_in(
                &doc["components"]["schemas"][&component],
                &component,
                false,
                &mut found,
            );
        }
        assert!(
            found.is_empty(),
            "{name}: an answer-side schema carries `default`: {found:#?}"
        );

        // The request side keeps its defaults: some component or inline
        // schema a request reaches has one.
        let mut kept = Vec::new();
        requests
            .iter()
            .for_each(|r| defaults_in(r, "request", false, &mut kept));
        for component in reach(doc, &requests) {
            defaults_in(
                &doc["components"]["schemas"][&component],
                &component,
                false,
                &mut kept,
            );
        }
        assert!(!kept.is_empty(), "{name}: no request-side default is left");
        // A known one: the run-open body's `phase`.
        assert!(
            doc["paths"]["/api/agents/{id}/runs"]["post"]["requestBody"]["content"]
                ["application/json"]["schema"]["properties"]["phase"]
                .get("default")
                .is_some(),
            "{name}: POST /api/agents/{{id}}/runs lost the default of `phase`"
        );
    }
}

#[test]
fn a_run_conflict_is_a_documented_409_with_its_own_schema() {
    for (name, doc) in [("admin", admin_doc()), ("v1", v1_doc())] {
        let op = &doc["paths"]["/api/agents/{id}/runs"]["post"];
        let responses = &op["responses"];
        let body = &responses["409"]["content"]["application/json"]["schema"];
        let text = body.to_string();
        assert!(
            text.contains("AgentRunConflict"),
            "{name}: POST /api/agents/{{id}}/runs documents no 409 AgentRunConflict: {responses}"
        );
        let schema = match body["$ref"]
            .as_str()
            .and_then(|r| r.strip_prefix("#/components/schemas/"))
        {
            Some(target) => &doc["components"]["schemas"][target],
            None => body,
        };
        for field in ["code", "message", "run"] {
            assert!(
                schema["properties"][field].is_object(),
                "{name}: the 409 schema has no `{field}`: {schema}"
            );
        }
        assert!(
            responses["default"].is_object(),
            "{name}: the dialect's default error is kept beside the 409"
        );
    }
}

#[test]
fn the_dictation_route_documents_the_fallback_headers_of_a_failed_call() {
    let op = &admin_doc()["paths"]["/chat/api/threads/{id}/transcribe"]["post"];
    assert!(
        op.is_object(),
        "the admin document lacks the transcribe route"
    );
    for status in ["200", "default"] {
        let headers = &op["responses"][status]["headers"];
        assert!(
            headers["x-lmgw-fallback"].is_object(),
            "the transcribe {status} response lists no x-lmgw-fallback: {headers}"
        );
    }
}

#[test]
fn the_embeddings_usage_is_typed_like_rerank_s() {
    for (name, doc) in [("admin", admin_doc()), ("v1", v1_doc())] {
        let schema = &doc["components"]["schemas"]["EmbeddingsResponse"];
        if name == "admin" {
            assert!(
                schema.is_object(),
                "the admin document lacks EmbeddingsResponse"
            );
        }
        if schema.is_null() {
            continue;
        }
        let usage = &schema["properties"]["usage"];
        assert_eq!(
            usage["required"],
            serde_json::json!(["prompt_tokens", "total_tokens"]),
            "{name}: {usage}"
        );
        assert!(usage["additionalProperties"].is_null(), "{name}: {usage}");
    }
}
