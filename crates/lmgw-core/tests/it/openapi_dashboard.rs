//! Plane checks for `/api`'s dashboard, usage, docs, agents and session
//! routes (api-docs design §7.1): every documented route is a
//! `CAPABILITY_TABLE` row, and each `query` schema has the handler's field
//! names. WP4.
//!
//! The three internal mini-APIs (Chat, Audio lab, Image lab) that used to be
//! `labs`'s plane are excluded entirely as of the 2026-09-28 owner decision
//! (`openapi/exclusions.rs`) — `openapi_coverage.rs` (WP8) is what proves
//! every `/chat/api`, `/audio-lab/api` and `/image-lab/api` row is covered
//! there, so this file no longer checks them.
//!
//! The last section (`json_or_sse_*`) is not a plane check: it pins the
//! registry/build.rs `Resp::JsonOrSse` gap this work package also closed
//! (design §"Extra") — the four inference routes it applies to live in
//! `openapi/planes/inference.rs` (WP6's file), so there is no dashboard-plane
//! home for that assertion; this is the file WP4 owns.

use std::collections::BTreeSet;

use lmgw_api_types::openapi_ext as ext;
use lmgw_core::openapi::admin_doc;
use lmgw_core::server::CAPABILITY_TABLE;

/// The one path prefix this work package's planes document — everything
/// under it except `POST /api/op/{name}` (the op table's expansion, not a
/// `DocRoute`; WP5's plane) belongs to `dashboard`/`usage`/`docs`/`agents`/
/// `session`.
const PREFIXES: &[&str] = &["/api"];

fn is_ours(path: &str) -> bool {
    PREFIXES
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
        // The op table's expansion (WP5): `/api/op/{name}` itself (the
        // `CAPABILITY_TABLE` row) and, now that `ops/table.rs` has real
        // entries, every concrete `/api/op/<name>` path the build expands it
        // into — none of those is its own `CAPABILITY_TABLE` row.
        && !path.starts_with("/api/op/")
        // The Knowledge page's backend is a dashboard backend, excluded with
        // DASHBOARD_BACKEND rows like the Chat's (chat-complete §9.5).
        && !path.starts_with("/api/knowledge/")
}

fn op(doc: &serde_json::Value, method: &str, path: &str) -> serde_json::Value {
    doc["paths"][path][method].clone()
}

// ---------------------------------------------------------------------------
// Coverage: every documented route of these planes is a CAPABILITY_TABLE row,
// and vice versa (within the four prefixes only — the reverse direction over
// every route, ops included, is WP8's `openapi_coverage.rs`).
// ---------------------------------------------------------------------------

#[test]
fn every_documented_plane_route_is_a_capability_table_row() {
    let doc = admin_doc();
    let table: BTreeSet<(String, String)> = CAPABILITY_TABLE
        .iter()
        .map(|(m, p, _)| (m.to_ascii_uppercase(), p.to_string()))
        .collect();

    let mut checked = 0;
    for (path, methods) in doc["paths"].as_object().unwrap() {
        if !is_ours(path) {
            continue;
        }
        for method in methods.as_object().unwrap().keys() {
            let row = (method.to_ascii_uppercase(), path.clone());
            assert!(
                table.contains(&row),
                "{method} {path} is documented but is not a CAPABILITY_TABLE row"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 60,
        "only checked {checked} operations — a plane went missing"
    );
}

#[test]
fn every_capability_table_row_under_our_prefixes_is_documented() {
    let doc = admin_doc();
    for (method, path, _) in CAPABILITY_TABLE {
        if !is_ours(path) {
            continue;
        }
        let method = method.to_ascii_lowercase();
        assert!(
            doc["paths"][path].get(&method).is_some(),
            "CAPABILITY_TABLE row {method} {path} is not documented by any plane"
        );
    }
}

// ---------------------------------------------------------------------------
// Query schemas name the handler's own fields.
// ---------------------------------------------------------------------------

fn query_props(doc: &serde_json::Value, method: &str, path: &str) -> BTreeSet<String> {
    op(doc, method, path)["parameters"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter(|p| p["in"] == "query")
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn dashboard_query_schemas_match_the_handler_field_names() {
    let doc = admin_doc();

    assert_eq!(
        query_props(doc, "get", "/api/logs"),
        [
            "limit",
            "errors_only",
            "alias",
            "upstream",
            "before_id",
            "key_id",
            "error_kind",
            "class",
            "alias_q",
            "upstream_q",
        ]
        .map(String::from)
        .into_iter()
        .collect(),
    );

    assert_eq!(
        query_props(doc, "get", "/api/local-model"),
        ["id", "model_id", "target"]
            .map(String::from)
            .into_iter()
            .collect(),
    );
}

#[test]
fn usage_query_schemas_match_the_handler_field_names() {
    let doc = admin_doc();
    assert_eq!(
        query_props(doc, "get", "/api/usage/series"),
        [
            "from",
            "to",
            "bucket",
            "group_by",
            "class",
            "alias",
            "key_id",
            "upstream_id",
            "tz",
            "limit",
        ]
        .map(String::from)
        .into_iter()
        .collect(),
    );
}

#[test]
fn docs_query_schemas_match_the_handler_field_names() {
    let doc = admin_doc();
    assert_eq!(
        query_props(doc, "get", "/api/docs/chunks"),
        ["document_id"].map(String::from).into_iter().collect(),
    );
    assert_eq!(
        query_props(doc, "get", "/api/docs/golden/candidates"),
        ["corpus_id", "status"]
            .map(String::from)
            .into_iter()
            .collect(),
    );
}

// ---------------------------------------------------------------------------
// AgentSelf/Ledger rows carry the agent-runtime tag, Admin rows the agents
// tag — the tag follows the capability, not the path (tags.rs).
// ---------------------------------------------------------------------------

#[test]
fn agent_runtime_capability_rows_carry_the_agent_runtime_tag() {
    let doc = admin_doc();
    for (method, path) in [
        ("get", "/api/agents/{id}"),
        ("get", "/api/agents/{id}/runs"),
        ("post", "/api/agents/{id}/runs"),
        ("get", "/api/agents/runs/{job_id}"),
        ("post", "/api/agents/runs/{job_id}/events"),
        ("post", "/api/agents/runs/{job_id}/close"),
    ] {
        let operation = op(doc, method, path);
        assert_eq!(
            operation["tags"],
            serde_json::json!(["agent-runtime"]),
            "{method} {path}"
        );
    }
    for (method, path) in [
        ("get", "/api/agents"),
        ("post", "/api/agents/import"),
        ("get", "/api/agents/{id}/export"),
    ] {
        let operation = op(doc, method, path);
        assert_eq!(
            operation["tags"],
            serde_json::json!(["agents"]),
            "{method} {path}"
        );
    }
}

// ---------------------------------------------------------------------------
// GET /api/openapi.json — the admin document, self-described.
// ---------------------------------------------------------------------------

#[test]
fn admin_openapi_json_is_documented_as_doc() {
    let doc = admin_doc();
    let operation = op(doc, "get", "/api/openapi.json");
    assert_eq!(operation[ext::CAPABILITY], "admin");
    assert!(operation["responses"]["200"]["content"]["application/json"].is_object());
}

// ---------------------------------------------------------------------------
// `Resp::JsonOrSse` (design "Extra"): both media types on the one 200, with
// the SSE side carrying x-lmgw-sse-events like `Resp::Sse`'s does.
// ---------------------------------------------------------------------------

#[test]
fn json_or_sse_routes_document_both_media_types() {
    let doc = admin_doc();
    for (method, path) in [
        ("post", "/v1/chat/completions"),
        ("post", "/v1/completions"),
        ("post", "/v1/messages"),
        ("post", "/v1/responses"),
    ] {
        let operation = op(doc, method, path);
        let response_200 = &operation["responses"]["200"];
        let content = response_200["content"]
            .as_object()
            .unwrap_or_else(|| panic!("{method} {path} has no 200 content: {response_200:#?}"));
        assert!(
            content.contains_key("application/json"),
            "{method} {path}: {content:#?}"
        );
        assert!(
            content.contains_key("text/event-stream"),
            "{method} {path}: {content:#?}"
        );
        let events = response_200[ext::SSE_EVENTS]
            .as_object()
            .unwrap_or_else(|| {
                panic!(
                    "{method} {path} has no {}: {response_200:#?}",
                    ext::SSE_EVENTS
                )
            });
        assert!(!events.is_empty(), "{method} {path}");
        assert!(
            operation["responses"].get("default").is_some(),
            "{method} {path} has no default response"
        );
    }
}
