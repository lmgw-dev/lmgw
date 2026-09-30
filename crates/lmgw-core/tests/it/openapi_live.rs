//! Live validation (api-docs design §7.2): every documented `/api` GET that
//! can be exercised in the test gateway, run for real and checked against its
//! own 200 schema; the `/api/events` SSE frames; `/v1/models` in both
//! dialects; the two `openapi.json` routes themselves. WP8.
//!
//! A validation failure here means either the doc or the DTO is wrong
//! (§7.2's own rule) — this file is the one place that finds that class of
//! bug, since every other openapi suite only ever reads the *document*, never
//! runs a route and checks its answer against what the document promised.

use crate::common;

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use lmgw_api_types::openapi_ext as ext;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::openapi::{admin_doc, v1_doc};
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAlias, NewLocalModel, NewUpstream};

use common::Gw;

// ---------------------------------------------------------------------------
// Fixtures (§7.2): a wiremock upstream answering /models and
// /chat/completions, an alias, one chat request sent through it, a stored
// local chat row, the seeded agents (`AppState::init_for_tests` already
// restores them, the same as a fresh install).
// ---------------------------------------------------------------------------

struct Fixtures {
    gw: Gw,
    upstream_id: i64,
    local_model_id: i64,
    agent_id: String,
    // Held alive for the fixture's lifetime: `local_model_id`'s gguf_path is
    // resolved under this directory, and GET /api/gguf-files needs a
    // configured, existing models dir to answer 200 rather than "not
    // configured".
    _models_dir: tempfile::TempDir,
    _mock: MockServer,
}

async fn fixtures() -> Fixtures {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "fixture-upstream-model"}],
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-live-fixture",
            "object": "chat.completion",
            "model": "tgt-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "pong"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2},
        })))
        .mount(&mock)
        .await;

    let state = AppState::init_for_tests().await.unwrap();

    // `GET /api/docs/export/manifest` copies the corpus DB *file*
    // (`quickdoc/portability.rs::export`) — `init_for_tests` deliberately
    // opens `state.corpus` in memory, so that file never exists on its own.
    // A real, empty, migrated file at the exact path production always has
    // one at is enough for the copy and the manifest read; nothing else here
    // queries it; `state.corpus` (in memory) is still what every handler
    // that lists or searches corpora actually uses.
    let corpus_file =
        quickdoc_core::store::open(&state.data_dir.join(lmgw_core::quickdoc::CORPUS_DB_FILE))
            .await
            .unwrap();
    quickdoc_core::store::checkpoint(&corpus_file)
        .await
        .unwrap();
    corpus_file.close().await;

    let upstream_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "live-fixture".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: mock.uri(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "live-fixture-chat".into(),
            upstream_id,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();

    let models_dir = tempfile::tempdir().unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.router.models_dir = models_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();

    let local_model_id = store::insert_local_model(
        &state.db,
        &NewLocalModel {
            model_id: "live-fixture-local".into(),
            gguf_path: "live-fixture.gguf".into(),
            params: Default::default(),
            args: vec![],
            idle_seconds: 300,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
    )
    .await
    .unwrap();

    state.reload_snapshot().await.unwrap();
    let gw = common::serve(state.clone()).await;

    // One chat request through the alias, so /api/logs and /api/usage/* have
    // a real row to answer rather than an empty (but still schema-valid)
    // shape.
    let resp = gw
        .client()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({
            "model": "live-fixture-chat",
            "messages": [{"role": "user", "content": "ping"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "fixture chat request must succeed");

    let agent_id = lmgw_core::agents::seed::shipped_all()
        .first()
        .expect("at least one shipped agent")
        .id
        .clone();

    Fixtures {
        gw,
        upstream_id,
        local_model_id,
        agent_id,
        _models_dir: models_dir,
        _mock: mock,
    }
}

/// `{local_model_id}` / `{upstream_id}` / `{agent_id}` placeholders in a
/// `Case::Get` template, resolved against the running fixture.
fn resolve(template: &str, fx: &Fixtures) -> String {
    template
        .replace("{local_model_id}", &fx.local_model_id.to_string())
        .replace("{upstream_id}", &fx.upstream_id.to_string())
        .replace("{agent_id}", &fx.agent_id)
}

// ---------------------------------------------------------------------------
// CASES (§7.2): every `/api` GET operationId, either a real request to make
// or a reason it cannot run here — kept in the same both-ways discipline as
// the rest of `openapi_coverage.rs`, scoped to `/api` (`GET /v1/models` and
// the two `openapi.json` routes are validated by their own tests below,
// outside this table).
// ---------------------------------------------------------------------------

enum Case {
    /// A path (with a literal query string, `{..}` fixture placeholders
    /// resolved before the request).
    Get(&'static str),
    Skip(&'static str),
}

const CASES: &[(&str, Case)] = &[
    ("get_api_version", Case::Get("/api/version")),
    (
        "get_api_openapi_json",
        Case::Skip("validated in full, byte for byte, by openapi_json_routes_serve_the_doc"),
    ),
    ("get_api_connect", Case::Get("/api/connect")),
    ("get_api_status", Case::Get("/api/status")),
    ("get_api_logs", Case::Get("/api/logs")),
    (
        "get_api_events",
        Case::Skip("SSE frames validated separately by events_initial_frames_validate"),
    ),
    ("get_api_jobs", Case::Get("/api/jobs")),
    ("get_api_vram", Case::Get("/api/vram")),
    ("get_api_models_full", Case::Get("/api/models/full")),
    (
        "get_api_local_model",
        Case::Get("/api/local-model?id={local_model_id}"),
    ),
    ("get_api_local_model_check", Case::Skip("untyped")),
    ("get_api_gguf_files", Case::Get("/api/gguf-files")),
    ("get_api_model_inspect", Case::Skip("untyped")),
    (
        "get_api_local_model_plan",
        Case::Skip("needs a real GGUF file to read metadata from"),
    ),
    (
        "get_api_ladder_rung_plan",
        Case::Skip("needs a real GGUF file to read metadata from"),
    ),
    ("get_api_llama_flags", Case::Skip("untyped")),
    ("get_api_upstreams", Case::Get("/api/upstreams")),
    ("get_api_wiring", Case::Get("/api/wiring")),
    ("get_api_mcp_servers", Case::Get("/api/mcp-servers")),
    (
        "get_api_mcp_servers_id_tools",
        Case::Skip("needs a connectable MCP server"),
    ),
    ("get_api_tools", Case::Get("/api/tools")),
    (
        "get_api_upstream_models",
        Case::Get("/api/upstream-models?id={upstream_id}"),
    ),
    (
        "get_api_hf_repo",
        Case::Skip("needs the real Hugging Face hub"),
    ),
    ("get_api_hf_downloads", Case::Get("/api/hf/downloads")),
    ("get_api_audio_catalog", Case::Get("/api/audio/catalog")),
    ("get_api_settings_full", Case::Get("/api/settings-full")),
    ("get_api_responses", Case::Get("/api/responses")),
    (
        "get_api_responses_chain",
        Case::Skip("needs a stored response"),
    ),
    ("get_api_usage_series", Case::Get("/api/usage/series")),
    ("get_api_usage_top", Case::Get("/api/usage/top")),
    ("get_api_usage_heat", Case::Get("/api/usage/heat")),
    ("get_api_usage_errors", Case::Get("/api/usage/errors")),
    ("get_api_usage_local", Case::Get("/api/usage/local")),
    ("get_api_usage_keys", Case::Get("/api/usage/keys")),
    ("get_api_usage_prices", Case::Get("/api/usage/prices")),
    (
        "get_api_usage_export_csv",
        Case::Skip("binary CSV, not JSON — content type only"),
    ),
    ("get_api_agents", Case::Get("/api/agents")),
    ("get_api_agents_id", Case::Get("/api/agents/{agent_id}")),
    ("get_api_agents_id_export", Case::Skip("untyped")),
    (
        "get_api_agents_id_runs",
        Case::Get("/api/agents/{agent_id}/runs"),
    ),
    (
        "get_api_agents_runs_job_id",
        Case::Skip("needs a live or stored agent run"),
    ),
    ("get_api_docs_corpora", Case::Get("/api/docs/corpora")),
    ("get_api_docs_corpora_id", Case::Skip("needs a corpus")),
    (
        "get_api_docs_corpora_id_documents",
        Case::Skip("needs a corpus"),
    ),
    ("get_api_docs_chunks", Case::Skip("needs a corpus")),
    ("get_api_docs_golden", Case::Skip("needs a corpus")),
    (
        "get_api_docs_golden_candidates",
        Case::Skip("needs a corpus"),
    ),
    ("get_api_docs_eval", Case::Skip("needs a corpus")),
    ("get_api_docs_requests", Case::Get("/api/docs/requests")),
    (
        "get_api_docs_export",
        Case::Skip("binary SQLite, not JSON — content type only"),
    ),
    (
        "get_api_docs_export_manifest",
        Case::Get("/api/docs/export/manifest"),
    ),
    (
        "get_api_session_login",
        Case::Skip("302 redirect, not a JSON body"),
    ),
    ("get_api_session", Case::Get("/api/session")),
];

/// Every `GET /api/*` operationId the admin document actually carries.
fn api_get_operation_ids(doc: &Value) -> BTreeSet<String> {
    doc["paths"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(path, _)| path.starts_with("/api"))
        .filter_map(|(_, methods)| methods.get("get"))
        .map(|op| op["operationId"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn every_get_has_a_case() {
    let doc = admin_doc();
    let documented = api_get_operation_ids(doc);
    let cased: BTreeSet<String> = CASES.iter().map(|(id, _)| id.to_string()).collect();
    assert_eq!(
        documented, cased,
        "CASES and the admin doc's /api GET operations have drifted"
    );
    assert_eq!(
        CASES.len(),
        53,
        "the /api GET count moved — update this file's count comment"
    );

    for (operation_id, case) in CASES {
        if let Case::Skip(reason) = case {
            assert!(
                !reason.trim().is_empty(),
                "{operation_id} is skipped with no reason"
            );
        }
    }
}

fn operation_by_id<'a>(doc: &'a Value, operation_id: &str) -> &'a Value {
    doc["paths"]
        .as_object()
        .unwrap()
        .values()
        .find_map(|methods| {
            methods
                .get("get")
                .filter(|op| op["operationId"] == operation_id)
        })
        .unwrap_or_else(|| panic!("no GET operation with id {operation_id}"))
}

/// `schema`, with `components` merged in so a `$ref` — bare or nested —
/// resolves as a JSON Pointer against this synthetic root (the same trick
/// `openapi_coverage.rs`'s `every_example_validates_against_its_schema` uses).
fn validator_root(schema: &Value, components: &Value) -> Value {
    let mut root = schema.clone();
    if let Value::Object(map) = &mut root {
        map.insert("components".to_string(), components.clone());
    }
    root
}

#[tokio::test]
async fn every_get_case_validates() {
    let fx = fixtures().await;
    let doc = admin_doc();
    let components = doc["components"].clone();

    let mut checked = 0;
    for (operation_id, case) in CASES {
        let Case::Get(template) = case else {
            continue;
        };
        let url = format!("{}{}", fx.gw, resolve(template, &fx));
        let resp = fx
            .gw
            .client()
            .get(&url)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{operation_id} ({url}): {e}"));
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .unwrap_or_else(|e| panic!("{operation_id} ({url}): body was not JSON: {e}"));
        assert_eq!(status, 200, "{operation_id} ({url}): {body:#?}");

        let operation = operation_by_id(doc, operation_id);
        let schema = operation
            .pointer("/responses/200/content/application~1json/schema")
            .unwrap_or_else(|| panic!("{operation_id} has no 200 application/json schema"));
        let root = validator_root(schema, &components);
        let result = jsonschema::validate(&root, &body);
        assert!(
            result.is_ok(),
            "{operation_id} ({url}): response does not validate against its documented \
             schema — either the doc or the DTO is wrong: {:#?}\nbody: {body:#?}",
            result.err()
        );
        checked += 1;
    }
    assert!(checked > 25, "only checked {checked} live GET cases");
}

// ---------------------------------------------------------------------------
// events_initial_frames_validate
// ---------------------------------------------------------------------------

struct SseFrame {
    event: String,
    data: String,
}

/// Every complete (`\n\n`-terminated) `event:`/`data:` block in `buf` — a
/// trailing partial block (still being written) is left for the next read,
/// exactly like a real SSE client would.
fn parse_sse_frames(buf: &str) -> Vec<SseFrame> {
    let mut frames = Vec::new();
    let mut rest = buf;
    while let Some(at) = rest.find("\n\n") {
        let block = &rest[..at];
        rest = &rest[at + 2..];
        let mut event = None;
        let mut data_lines = Vec::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("data:") {
                data_lines.push(v.trim_start().to_string());
            }
        }
        if let Some(event) = event {
            frames.push(SseFrame {
                event,
                data: data_lines.join("\n"),
            });
        }
    }
    frames
}

#[tokio::test]
async fn events_initial_frames_validate() {
    const INITIAL: &[&str] = &["stats", "jobs", "runtime", "vram", "mcp", "updates"];

    let fx = fixtures().await;
    let doc = admin_doc();
    let components = doc["components"].clone();
    let operation = doc["paths"]["/api/events"]["get"].clone();
    let sse_events = operation["responses"]["200"][ext::SSE_EVENTS]
        .as_object()
        .expect("GET /api/events documents x-lmgw-sse-events");

    let mut resp = fx
        .gw
        .client()
        .get(format!("{}/api/events", fx.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let frames = loop {
        let frames = parse_sse_frames(&buf);
        let have: BTreeSet<&str> = frames.iter().map(|f| f.event.as_str()).collect();
        if INITIAL.iter().all(|n| have.contains(n)) {
            break frames;
        }
        let chunk = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .unwrap_or_else(|_| {
                panic!("timed out waiting for the initial frames; got so far:\n{buf}")
            })
            .unwrap()
            .expect("the stream stays open");
        buf.push_str(&String::from_utf8_lossy(&chunk));
    };

    let mut checked = 0;
    for name in INITIAL {
        let frame = frames
            .iter()
            .find(|f| f.event == *name)
            .unwrap_or_else(|| panic!("no {name} frame: {buf}"));
        let data: Value = serde_json::from_str(&frame.data)
            .unwrap_or_else(|e| panic!("{name} frame data is not JSON ({e}): {}", frame.data));
        let schema = sse_events
            .get(*name)
            .unwrap_or_else(|| panic!("x-lmgw-sse-events has no entry for {name}"));
        let root = validator_root(schema, &components);
        let result = jsonschema::validate(&root, &data);
        assert!(
            result.is_ok(),
            "{name} frame does not validate against its schema: {:#?}\ndata: {data:#?}",
            result.err()
        );
        checked += 1;
    }
    assert_eq!(checked, INITIAL.len());
}

// ---------------------------------------------------------------------------
// v1_models_validates_in_both_dialects
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v1_models_validates_in_both_dialects() {
    let fx = fixtures().await;
    let doc = admin_doc();
    let components = doc["components"].clone();

    let list_schema = doc["paths"]["/v1/models"]["get"]
        .pointer("/responses/200/content/application~1json/schema")
        .unwrap()
        .clone();
    let by_id_schema = doc["paths"]["/v1/models/{id}"]["get"]
        .pointer("/responses/200/content/application~1json/schema")
        .unwrap()
        .clone();

    for anthropic in [false, true] {
        for (path, schema) in [
            ("/v1/models".to_string(), &list_schema),
            ("/v1/models/live-fixture-chat".to_string(), &by_id_schema),
        ] {
            let mut req = fx.gw.client().get(format!("{}{path}", fx.gw));
            if anthropic {
                req = req.header("anthropic-version", "2023-06-01");
            }
            let resp = req.send().await.unwrap();
            assert_eq!(resp.status(), 200, "{path} anthropic={anthropic}");
            let body: Value = resp.json().await.unwrap();
            let root = validator_root(schema, &components);
            let result = jsonschema::validate(&root, &body);
            assert!(
                result.is_ok(),
                "{path} anthropic={anthropic}: {:#?}\nbody: {body:#?}",
                result.err()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The two openapi.json routes: served byte for byte from the same cached
// document the in-process helpers build (§4.11 "built once into an
// OnceLock<Value>").
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openapi_json_routes_serve_the_doc() {
    let fx = fixtures().await;

    let admin_resp = fx
        .gw
        .client()
        .get(format!("{}/api/openapi.json", fx.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(admin_resp.status(), 200);
    let admin_body: Value = admin_resp.json().await.unwrap();
    assert_eq!(&admin_body, admin_doc());

    let v1_resp = fx
        .gw
        .client()
        .get(format!("{}/v1/openapi.json", fx.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(v1_resp.status(), 200);
    let v1_body: Value = v1_resp.json().await.unwrap();
    assert_eq!(&v1_body, v1_doc());
}
