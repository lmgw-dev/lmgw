//! Dashboard-plane smoke tests: the SPA is served at `/` (with the old `/ui`
//! mount redirecting there), the `/api` plane answers what the pages need, and
//! `/v1/models` reflects auto-exposed public local models.
//!
//! These go through the real router over HTTP, so a route that stopped being
//! wired up fails here rather than in the browser.

use lmgw_core::config::{AuxKind, HoldFallbackMode, Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewAudioModel, NewAuxModel, NewLocalModel, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;
use common::{serve, Gw};

async fn get(base: &Gw, path: &str) -> (u16, String) {
    let resp = base
        .client()
        .get(format!("{base}{path}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap_or_default())
}

async fn get_json(base: &Gw, path: &str) -> Value {
    let (status, body) = get(base, path).await;
    assert_eq!(status, 200, "GET {path}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("GET {path} is not JSON ({e}): {body}"))
}

/// `POST /api/op/{name}` → `(status, parsed body)`.
async fn op(base: &Gw, name: &str, args: Value) -> (u16, Value) {
    let resp = base
        .client()
        .post(format!("{base}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let v = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("op {name} is not JSON ({e}): {body}"));
    (status, v)
}

fn local(model_id: &str, gguf: &str) -> NewLocalModel {
    NewLocalModel {
        model_id: model_id.into(),
        gguf_path: gguf.into(),
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
    }
}

/// The SPA owns `/` and every client-side route under it; the `/api` reads
/// behind those pages answer; `/v1/models` sees the public local model.
/// The argv one chat model renders to, straight from the stored row — the
/// successor of the INI-preset assertions this file used to make (§7).
fn chat_argv(m: lmgw_core::config::LocalModel) -> Vec<String> {
    use lmgw_core::config::Snapshot;
    use lmgw_core::runtime::argv::render_llama_args;
    use lmgw_core::runtime::descriptor::model_runtime;
    use lmgw_core::runtime::Class;

    let model_id = m.model_id.clone();
    let snap = Snapshot {
        local_models: vec![m],
        ..Snapshot::default()
    };
    let rt = model_runtime(&snap, Class::Chat, &model_id).unwrap();
    let spec = rt
        .render_spec("lmgw", 9001, "/srv/models", std::path::Path::new("/tmp"))
        .unwrap();
    render_llama_args(&spec)
}

/// A `--flag value` pair, adjacent, anywhere in the argv.
fn has_pair(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2).any(|w| w[0] == flag && w[1] == value)
}

#[tokio::test]
async fn the_spa_is_served_at_the_root_and_the_api_backs_its_pages() {
    let state = AppState::init_for_tests().await.unwrap();
    store::insert_local_model(&state.db, &local("gemma4-12b", "gemma-4-12B.gguf"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;

    // Client-side routes all resolve to the SPA shell (index.html), so a deep
    // link or a reload lands on the same page a click would.
    for path in [
        "/",
        "/chat",
        "/audio-lab",
        "/image-lab",
        "/agents",
        // Both shipped agents, one per run kind: the chat preset and the batch
        // pipeline the retired mail workflow became (agent-catalog §8).
        "/agents/docs-librarian",
        "/agents/mail-labeler",
        // The old Workflows deep link is a client-side redirect to `/agents`,
        // so it still has to be served the shell even though the page behind
        // it is gone.
        "/workflows",
        "/models",
        "/models/local/new",
        "/downloads",
        "/upstreams",
        "/mcp-servers",
        "/traffic",
        "/wiring",
        "/settings",
    ] {
        let (status, body) = get(&base, path).await;
        assert_eq!(status, 200, "GET {path}");
        assert!(
            body.contains("<!DOCTYPE html>") || body.contains("lmgw-ui bundle missing"),
            "GET {path} did not serve the SPA shell: {}",
            &body[..body.len().min(120)]
        );
    }

    // The reads each page opens with.
    for path in [
        "/api/status",
        "/api/connect",
        "/api/models/full",
        "/api/upstreams",
        "/api/mcp-servers",
        "/api/wiring",
        "/api/logs",
        "/api/responses",
        "/api/settings-full",
        "/api/hf/downloads",
        // The agent catalog's two pages (agent-catalog §5): the cards, one
        // agent in full, and its runs.
        "/api/agents",
        "/api/agents/docs-librarian",
        "/api/agents/docs-librarian/runs",
        // The two user-facing mini-APIs behind the labs (image-generation §8):
        // their pages open with a model list, and a lab whose plane stopped
        // being wired up is a page with an empty picker.
        "/audio-lab/api/models",
        "/image-lab/api/models",
    ] {
        get_json(&base, path).await;
    }

    let v = get_json(&base, "/v1/models").await;
    let entry = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"].as_str() == Some("gemma4-12b"))
        .unwrap_or_else(|| panic!("public local model missing from /v1/models: {v}"));
    // Local models run on our own hardware, so they advertise zero price.
    assert_eq!(entry["pricing"]["prompt"].as_str(), Some("0"));
    assert_eq!(entry["pricing"]["completion"].as_str(), Some("0"));

    // The public local routes without any alias/upstream rows.
    let route = state.snapshot().resolve("gemma4-12b").unwrap();
    assert_eq!(route.upstream_model, "gemma4-12b");
}

/// The three ledger routes are registered and reachable (container-runtime
/// §3.2, §8).
///
/// A refusal rather than a 404 is the whole assertion: `runs` is a reserved
/// agent id, so `/api/agents/runs/{run}/events` and `/api/agents/{id}/runs`
/// share a prefix, and a route-ordering slip would make one of them answer as
/// the other — which a test that only exercised the happy path would never see.
#[tokio::test]
async fn the_run_ledger_routes_are_on_the_table_and_answer_their_own_refusal() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state).await;
    for path in [
        "/api/agents/docs-librarian/runs",
        "/api/agents/runs/1/events",
        "/api/agents/runs/1/close",
    ] {
        // No credential at all: the gate's own 401 (principals §3.9).
        let resp = base
            .anon()
            .post(format!("{base}{path}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 401, "POST {path}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["code"], serde_json::json!("session_required"));

        // The owner's own session is refused by the **layer**: `Ledger` is
        // deliberately agent-only (§3.2), so an owner does not hold it at all
        // — a run is written by the agent that owns it. (`run_not_owned` is
        // the handler's neighbouring refusal, for an agent writing into
        // somebody else's run; `principal_gate.rs` holds both apart.)
        let resp = base
            .client()
            .post(format!("{base}{path}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 403, "POST {path}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["code"], serde_json::json!("forbidden"));
    }
}

/// The service-mode proxy routes are registered, and the SPA's own
/// `/agents/<id>` is **not** shadowed by them (container-runtime §3.3, §8).
///
/// The whole risk this covers is route ordering: `/agents/{id}/mcp`, the moved
/// app mount and the SPA's `/{*path}` catch-all all want these paths, and
/// `ui::API_PREFIXES` was deliberately not given an `agents/` entry — which
/// would have 404'd the agent detail page itself. A JSON body from the proxy
/// rather than the SPA shell is what proves the proxy got them.
#[tokio::test]
async fn the_service_proxy_routes_are_on_the_table_without_shadowing_the_spa() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state).await;

    for path in [
        "/agents/docs-librarian/app/",
        "/agents/docs-librarian/app/index.html",
        "/agents/docs-librarian/mcp",
    ] {
        let (status, body) = get(&base, path).await;
        assert_eq!(status, 404, "GET {path}: {body}");
        let v: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("GET {path} is not the proxy's JSON ({e}): {body}"));
        assert!(
            // The app mount is gone and says so (origins §4.2); the MCP face
            // is still a path, and this agent declares no `provides.mcp`.
            v["code"] == serde_json::json!("agent_app_moved")
                || v["code"] == serde_json::json!("agent_provides_no_mcp"),
            "GET {path}: {v}"
        );
    }

    // And the page the owner actually opens still gets the shell.
    let (status, body) = get(&base, "/agents/docs-librarian").await;
    assert_eq!(status, 200);
    assert!(
        body.contains("<!DOCTYPE html>") || body.contains("lmgw-ui bundle missing"),
        "the SPA lost its own detail page"
    );
}

/// Traffic's filter boxes match a *part* of the alias or upstream, while the
/// exact `alias=` a Usage chart links with stays exact — and a model id's
/// `_` or `%` is a character to find, not a LIKE wildcard.
#[tokio::test]
async fn the_log_filters_match_a_substring_only_when_asked_to() {
    let state = AppState::init_for_tests().await.unwrap();
    for (alias, upstream) in [
        ("gemma4-12b", "llama-server"),
        ("gemma4-12b-reason", "llama-server"),
        ("qwen", "kilo-gw"),
        ("a_b", "kilo-gw"),
        ("axb", "kilo-gw"),
        ("50%-off", "kilo-gw"),
    ] {
        store::insert_request_log(
            &state.db,
            &store::NewRequestLog {
                ingress_proto: "openai".into(),
                requested_alias: alias.into(),
                upstream_name: Some(upstream.into()),
                status: 200,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    let base = serve(state).await;
    let aliases = |v: Value| -> Vec<String> {
        let mut out: Vec<String> = v["logs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["requested_alias"].as_str().unwrap().to_string())
            .collect();
        out.sort();
        out
    };

    let v = get_json(&base, "/api/logs?alias_q=gemma").await;
    assert_eq!(aliases(v), ["gemma4-12b", "gemma4-12b-reason"]);
    // Case-insensitive, like the box it comes from.
    let v = get_json(&base, "/api/logs?alias_q=GEMMA4-12B-R").await;
    assert_eq!(aliases(v), ["gemma4-12b-reason"]);
    // The link's exact match: a prefix is not the alias.
    let v = get_json(&base, "/api/logs?alias=gemma").await;
    assert!(aliases(v).is_empty());
    let v = get_json(&base, "/api/logs?alias=gemma4-12b").await;
    assert_eq!(aliases(v), ["gemma4-12b"]);
    // `_` and `%` are literal.
    let v = get_json(&base, "/api/logs?alias_q=a_b").await;
    assert_eq!(aliases(v), ["a_b"]);
    let v = get_json(&base, "/api/logs?alias_q=%25").await;
    assert_eq!(aliases(v), ["50%-off"]);
    // The upstream box, and both boxes together.
    let v = get_json(&base, "/api/logs?upstream_q=llama").await;
    assert_eq!(aliases(v), ["gemma4-12b", "gemma4-12b-reason"]);
    let v = get_json(&base, "/api/logs?upstream_q=kilo&alias_q=b").await;
    assert_eq!(aliases(v), ["a_b", "axb"]);
    // An empty box is no filter.
    let v = get_json(&base, "/api/logs?alias_q=&upstream_q=").await;
    assert_eq!(aliases(v).len(), 6);
}

/// The request gate's clamp column (migration 0041) round-trips through
/// `/api/logs`: a row lmgw logged with `max_tokens_clamped` set shows it in
/// the JSON the Traffic page reads, and a row that was never clamped shows
/// `null` rather than `0` — the two must stay distinguishable all the way out.
#[tokio::test]
async fn max_tokens_clamped_round_trips_through_the_logs_api() {
    let state = AppState::init_for_tests().await.unwrap();
    store::insert_request_log(
        &state.db,
        &store::NewRequestLog {
            ingress_proto: "openai".into(),
            requested_alias: "ladder-model".into(),
            status: 200,
            max_tokens_clamped: Some(4096),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    store::insert_request_log(
        &state.db,
        &store::NewRequestLog {
            ingress_proto: "openai".into(),
            requested_alias: "plain-model".into(),
            status: 200,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let base = serve(state).await;
    let v = get_json(&base, "/api/logs").await;
    let rows = v["logs"].as_array().unwrap();
    let row = |alias: &str| {
        rows.iter()
            .find(|r| r["requested_alias"] == alias)
            .unwrap_or_else(|| panic!("no row for {alias}: {rows:?}"))
    };
    assert_eq!(row("ladder-model")["max_tokens_clamped"], 4096);
    assert!(row("plain-model")["max_tokens_clamped"].is_null());
}

/// The fallback-reason column (migration 0042) round-trips through
/// `/api/logs` and `lmgw__logs`: a row a fallback served says why, and a row
/// nothing re-routed says `null`.
#[tokio::test]
async fn fallback_reason_round_trips_through_both_log_readers() {
    let state = AppState::init_for_tests().await.unwrap();
    for (alias, reason) in [
        ("held-model", Some("hold")),
        ("crowded-model", Some("external_vram")),
        ("plain-model", None),
    ] {
        store::insert_request_log(
            &state.db,
            &store::NewRequestLog {
                ingress_proto: "openai".into(),
                requested_alias: alias.into(),
                status: 200,
                fallback_reason: reason.map(str::to_string),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    let mcp = lmgw_core::ops::logs(&state, None, None, None, None)
        .await
        .unwrap();
    let base = serve(state).await;
    let v = get_json(&base, "/api/logs").await;
    for (rows, alias_key) in [(&v["logs"], "requested_alias"), (&mcp["logs"], "alias")] {
        let rows = rows.as_array().unwrap();
        let row = |alias: &str| {
            rows.iter()
                .find(|r| r[alias_key] == alias)
                .unwrap_or_else(|| panic!("no row for {alias}: {rows:?}"))
        };
        assert_eq!(row("held-model")["fallback_reason"], "hold");
        assert_eq!(row("crowded-model")["fallback_reason"], "external_vram");
        assert!(row("plain-model")["fallback_reason"].is_null());
    }
}

/// The rung column (migration 0043) round-trips through `/api/logs` and
/// `lmgw__logs`, next to `fallback_reason` — same shape, same two readers.
/// The request path fills it from each send's lease
/// (`vram_admission/ladder_*.rs`); this pins that the column and both readers
/// carry whatever value a caller writes.
#[tokio::test]
async fn rung_round_trips_through_both_log_readers() {
    let state = AppState::init_for_tests().await.unwrap();
    for (alias, rung) in [("climbed-model", Some(2)), ("plain-model", None)] {
        store::insert_request_log(
            &state.db,
            &store::NewRequestLog {
                ingress_proto: "openai".into(),
                requested_alias: alias.into(),
                status: 200,
                rung,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    let mcp = lmgw_core::ops::logs(&state, None, None, None, None)
        .await
        .unwrap();
    let base = serve(state).await;
    let v = get_json(&base, "/api/logs").await;
    for (rows, alias_key) in [(&v["logs"], "requested_alias"), (&mcp["logs"], "alias")] {
        let rows = rows.as_array().unwrap();
        let row = |alias: &str| {
            rows.iter()
                .find(|r| r[alias_key] == alias)
                .unwrap_or_else(|| panic!("no row for {alias}: {rows:?}"))
        };
        assert_eq!(row("climbed-model")["rung"], 2);
        assert!(row("plain-model")["rung"].is_null());
    }
}

/// The live stream opens with every frame a dashboard needs to draw its
/// steady state — `mcp` included, which is published on change only: the
/// sidebar flags an MCP server in error from it, and one that failed before
/// the page opened must not wait for its next transition to be seen.
#[tokio::test]
async fn the_live_stream_opens_with_the_mcp_statuses() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state).await;
    let mut resp = base
        .client()
        .get(format!("{base}/api/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !seen.contains("event: mcp") {
        let chunk = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .unwrap_or_else(|_| panic!("no mcp frame on connect; got:\n{seen}"))
            .unwrap()
            .expect("the stream stays open");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    for frame in ["stats", "jobs", "runtime", "vram"] {
        assert!(
            seen.contains(&format!("event: {frame}")),
            "{frame}:\n{seen}"
        );
    }
}

/// `/ui` was the SPA's mount while the old UI still owned `/` (plan P0–P7).
/// It redirects permanently to the same path at the root, query string kept,
/// so bookmarks and the Tauri window's stored location survive the cutover.
#[tokio::test]
async fn the_old_ui_mount_redirects_to_the_root() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    for (from, to) in [
        ("/ui", "/"),
        ("/ui/", "/"),
        ("/ui/models", "/models"),
        ("/ui/chat?t=7", "/chat?t=7"),
    ] {
        let resp = client.get(format!("{base}{from}")).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 308, "GET {from}");
        assert_eq!(
            resp.headers().get("location").unwrap().to_str().unwrap(),
            to,
            "GET {from}"
        );
    }

    // The old askama pages are gone, and a miss under an API prefix is a 404
    // rather than the SPA shell — an API call must never be answered with HTML.
    for path in ["/local", "/hf", "/embed", "/logs", "/events"] {
        let (status, _) = get(&base, path).await;
        assert_eq!(status, 200, "SPA shell expected for {path}");
    }
    for path in [
        "/api/nope",
        "/chat/api/nope",
        "/audio-lab/api/nope",
        "/image-lab/api/nope",
    ] {
        let resp = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 404, "GET {path}");
    }
}

/// The local-model thinking controls, through the op the editor posts.
///
/// A patch field is bound by name, so a mismatch between the form and the
/// struct is not a compile error — it is a control that silently does nothing.
/// This posts what the editor posts and checks the preset that comes out.
#[tokio::test]
async fn local_model_set_saves_the_reasoning_preserve_and_kwargs_controls() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::insert_local_model(&state.db, &local("qwen3", "qwen3.gguf"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;

    let saved = || async {
        store::list_local_models(&state.db)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.model_id == "qwen3")
            .unwrap()
    };

    let (status, body) = op(
        &base,
        "local_model_set",
        json!({
            "action": "update", "id": id,
            "jinja": true,
            "reasoning_preserve": true,
            "reasoning_effort": "medium",
            "chat_template_kwargs": r#"{"preserve_thinking":true}"#,
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let m = saved().await;
    assert_eq!(m.params.reasoning_preserve, Some(true));
    assert_eq!(m.params.reasoning_effort.as_deref(), Some("medium"));
    let argv = chat_argv(m);
    // A bare switch, not `--reasoning-preserve true`: the CLI semantics the
    // preset hid (§3.6).
    assert!(argv.iter().any(|a| a == "--reasoning-preserve"), "{argv:?}");
    assert!(has_pair(&argv, "--reasoning-effort", "medium"), "{argv:?}");
    assert!(
        has_pair(
            &argv,
            "--chat-template-kwargs",
            r#"{"preserve_thinking":true}"#
        ),
        "{argv:?}"
    );

    // …and the editor's read endpoint hands all three back into their own
    // controls, rather than showing a default next to a value quietly in force.
    let v = get_json(&base, &format!("/api/local-model?id={id}")).await;
    assert_eq!(v["params"]["reasoning_preserve"], json!(true));
    assert_eq!(v["params"]["reasoning_effort"], json!("medium"));
    assert_eq!(
        v["params"]["chat_template_kwargs"]["preserve_thinking"],
        json!(true)
    );

    // The stored value survives a bad edit: malformed JSON is rejected with a
    // structured error, not written over the good one.
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "update", "id": id, "chat_template_kwargs": "{oops"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("JSON"),
        "{body}"
    );
    assert_eq!(
        saved()
            .await
            .params
            .chat_template_kwargs
            .get("preserve_thinking"),
        Some(&json!(true)),
        "a rejected edit overwrote the stored kwargs"
    );

    // Clearing hands the decision back to the chat template.
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({
            "action": "update", "id": id,
            "clear": "reasoning_preserve, reasoning_effort, chat_template_kwargs",
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let m = saved().await;
    assert_eq!(m.params.reasoning_preserve, None);
    assert!(m.params.chat_template_kwargs.is_empty());
    let argv = chat_argv(m);
    for gone in [
        "--reasoning-preserve",
        "--no-reasoning-preserve",
        "--reasoning-effort",
        "--chat-template-kwargs",
    ] {
        assert!(!argv.iter().any(|a| a == gone), "{gone} survived: {argv:?}");
    }
}

/// The prompt-state cache control — same reasoning as the test above, and `-1`
/// (no limit) is the value most worth being able to type.
#[tokio::test]
async fn local_model_set_saves_the_cache_ram_control() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::insert_local_model(&state.db, &local("qwen3", "qwen3.gguf"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;
    let saved = || async {
        store::list_local_models(&state.db)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.model_id == "qwen3")
            .unwrap()
    };

    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "update", "id": id, "cache_ram": -1}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let m = saved().await;
    assert_eq!(m.params.cache_ram, Some(-1));
    assert!(
        has_pair(&chat_argv(m), "--cache-ram", "-1"),
        "cache-ram did not reach the command line"
    );

    // Cleared = unset, which leaves llama.cpp's own default in force.
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "update", "id": id, "clear": "cache_ram"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let m = saved().await;
    assert_eq!(m.params.cache_ram, None);
    assert!(!chat_argv(m).iter().any(|a| a == "--cache-ram"));
}

/// The unified-KV controls (unified-KV design §3.1) — same "form field name
/// bound to a struct field" reasoning as the tests above, through the same
/// op the editor's select and per-slot-cap input post to.
#[tokio::test]
async fn local_model_set_saves_the_kv_unified_controls() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::insert_local_model(&state.db, &local("qwen3", "qwen3.gguf"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;
    let saved = || async {
        store::list_local_models(&state.db)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.model_id == "qwen3")
            .unwrap()
    };

    let (status, body) = op(
        &base,
        "local_model_set",
        json!({
            "action": "update", "id": id,
            "kv_unified": true, "kv_unified_per_slot": 8192, "parallel": 2, "n_predict": 4096
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let m = saved().await;
    assert_eq!(m.params.kv_unified, Some(true));
    assert_eq!(m.params.kv_unified_per_slot, Some(8192));
    let argv = chat_argv(m);
    assert!(argv.iter().any(|a| a == "--kv-unified"), "{argv:?}");
    assert!(has_pair(&argv, "--kv-unified-per-slot", "8192"), "{argv:?}");

    let v = get_json(&base, &format!("/api/local-model?id={id}")).await;
    assert_eq!(v["params"]["kv_unified"], json!(true));
    assert_eq!(v["params"]["kv_unified_per_slot"], json!(8192));

    // Decision D2: an explicitly shared pool with no bound is refused.
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "update", "id": id, "clear": "n_predict"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("n_predict"),
        "{body}"
    );
    // The refused edit must not have taken partial effect.
    assert_eq!(saved().await.params.n_predict, Some(4096));

    // Clearing both hands the row back to llama-server's own default.
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "update", "id": id, "clear": "kv_unified, kv_unified_per_slot"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let m = saved().await;
    assert_eq!(m.params.kv_unified, None);
    assert_eq!(m.params.kv_unified_per_slot, None);
    let argv = chat_argv(m);
    assert!(!argv.iter().any(|a| a.contains("kv-unified")), "{argv:?}");
}

/// The max-output-tokens control (`--n-predict`) — same reasoning as the
/// cache-ram test above, plus the editor's read endpoint, since this is the
/// number a later work package advertises as `/v1/models`' `max_output_tokens`.
#[tokio::test]
async fn local_model_set_saves_the_n_predict_control() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::insert_local_model(&state.db, &local("qwen3", "qwen3.gguf"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;
    let saved = || async {
        store::list_local_models(&state.db)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.model_id == "qwen3")
            .unwrap()
    };

    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "update", "id": id, "n_predict": 4096}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let m = saved().await;
    assert_eq!(m.params.n_predict, Some(4096));
    assert!(
        has_pair(&chat_argv(m), "--n-predict", "4096"),
        "n-predict did not reach the command line"
    );

    // The editor's read endpoint hands the value back into its own control.
    let v = get_json(&base, &format!("/api/local-model?id={id}")).await;
    assert_eq!(v["params"]["n_predict"], json!(4096));

    // Cleared = unset, which leaves llama.cpp's own default (-1, unbounded)
    // in force.
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "update", "id": id, "clear": "n_predict"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let m = saved().await;
    assert_eq!(m.params.n_predict, None);
    assert!(!chat_argv(m).iter().any(|a| a == "--n-predict"));
    let v = get_json(&base, &format!("/api/local-model?id={id}")).await;
    assert!(v["params"]["n_predict"].is_null(), "{v}");
}

/// The MCP config plane round-trips a server definition through the op →
/// store → snapshot → read endpoint, including JSON columns (args, env) and
/// the synthesized Podman argv. No southbound connection is made.
#[tokio::test]
async fn mcp_server_create_round_trips_through_config_plane() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;

    // A Podman-isolated stdio server with env + args + a tool prefix.
    let (status, body) = op(
        &base,
        "mcp_server_set",
        json!({
            "action": "create",
            "name": "github",
            "transport": "stdio",
            "container_image": "ghcr.io/acme/mcp-github",
            "command": "mcp-server-github",
            "args": "--verbose",
            "env": "GITHUB_TOKEN=secret123",
            "extra_run_args": "-v ./data:/data:Z",
            "tool_prefix": "gh",
            "timeout_ms": 45000,
            "autostart": true,
            "allow_sampling": true,
            "enabled": true,
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // The definition landed in the snapshot with JSON columns intact.
    let snap = state.snapshot();
    assert_eq!(snap.mcp_servers.len(), 1);
    let s = snap.mcp_servers.values().next().unwrap();
    assert_eq!(s.name, "github");
    assert_eq!(s.args, vec!["--verbose".to_string()]);
    assert_eq!(
        s.env,
        vec![("GITHUB_TOKEN".to_string(), "secret123".to_string())]
    );
    assert_eq!(
        s.extra_run_args,
        vec!["-v".to_string(), "./data:/data:Z".into()]
    );
    assert_eq!(s.tool_prefix, "gh");
    assert_eq!(s.timeout_ms, 45000);
    assert!(s.is_isolated() && s.allow_sampling && s.autostart && s.enabled);

    // The synthesized argv carries env as -e and the `:Z` bind mount intact.
    let (prog, argv) = s.stdio_argv();
    assert_eq!(prog, "podman");
    assert!(argv.contains(&"-e".to_string()));
    assert!(argv.contains(&"GITHUB_TOKEN=secret123".to_string()));
    assert!(argv.contains(&"./data:/data:Z".to_string()));
    assert!(argv.contains(&"ghcr.io/acme/mcp-github".to_string()));

    // The list endpoint reports it for the MCP page (secrets not included).
    let v = get_json(&base, "/api/mcp-servers").await;
    let row = v["mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == json!("github"))
        .unwrap_or_else(|| panic!("server missing from /api/mcp-servers: {v}"));
    assert_eq!(row["tool_prefix"], json!("gh"));

    // A bad transport/URL combination is a surfaced 400, not a 500.
    let (status, body) = op(
        &base,
        "mcp_server_set",
        json!({"action": "create", "name": "broken", "transport": "http"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("http/sse servers need a url"),
        "{body}"
    );
    assert_eq!(
        state.snapshot().mcp_servers.len(),
        1,
        "invalid server persisted"
    );
}

/// A wildcard bind (`0.0.0.0`) is unusable as a client URL, so the connect
/// panel's chips must expand it to loopback (plus any LAN addresses) instead of
/// handing out `http://0.0.0.0:…`.
///
/// The three fixed backend rows this used to assert went with router mode
/// (§7/§8): a direct URL belongs to a model's container now, on a port
/// allocated per start, and the `runtime` list carries it.
#[tokio::test]
async fn connect_offers_dialable_urls() {
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.bind_addr = "0.0.0.0:8001".into();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state).await;

    let v = get_json(&base, "/api/connect").await;
    let urls: Vec<String> = v["bases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["url"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !urls.iter().any(|u| u.contains("0.0.0.0")),
        "connect still advertises the wildcard bind address: {urls:?}"
    );
    assert!(
        urls.iter().any(|u| u == "http://127.0.0.1:8001"),
        "missing the loopback base URL: {urls:?}"
    );

    assert!(
        v.get("backends").is_none(),
        "the fixed backend rows are gone with router mode: {v}"
    );
}

/// The settings surface carries the per-model-containers class shape (§6):
/// image / models_dir / extra_run_args / public_prefix per class, plus the
/// global container_prefix — and nothing of router mode. The four removed keys
/// are a contract change with no compat shim, so a patch still sending one is
/// refused rather than silently ignored.
#[tokio::test]
async fn settings_full_speaks_the_class_shape_and_refuses_the_router_mode_keys() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;

    let v = get_json(&base, "/api/settings-full").await;
    assert_eq!(v["container_prefix"], json!("lmgw"));
    for section in ["router", "aux_router"] {
        let s = &v[section];
        for key in ["image", "models_dir", "extra_run_args", "public_prefix"] {
            assert!(s.get(key).is_some(), "{section}.{key} missing: {v}");
        }
        for gone in ["container_name", "listen_port", "models_max", "auto_start"] {
            assert!(s.get(gone).is_none(), "{section}.{gone} survived: {v}");
        }
    }
    // Audio keeps its four engine fields, which flow into every audio model's
    // own server.json (§3.6).
    for key in ["backend", "device", "threads", "lazy_load"] {
        assert!(v["audio"].get(key).is_some(), "audio.{key} missing: {v}");
    }
    assert!(v["audio"].get("container_name").is_none(), "{v}");

    // The new shape round-trips through the DTO the UI deserializes.
    let dto: lmgw_api_types::SettingsFull = serde_json::from_value(v).unwrap();
    assert_eq!(dto.router.image, "ghcr.io/ggml-org/llama.cpp:server-cuda");
    assert_eq!(dto.aux_router.public_prefix, "embed");
    assert_eq!(dto.audio.public_prefix, "audio");

    let (status, _) = op(
        &base,
        "settings_set_full",
        json!({"router": {"image": "localhost/mine:latest", "public_prefix": "local"}}),
    )
    .await;
    assert_eq!(status, 200);
    let s = state.snapshot().settings.router.clone();
    assert_eq!(s.image, "localhost/mine:latest");
    assert_eq!(s.public_prefix, "local");

    for gone in [
        json!({"router": {"container_name": "x"}}),
        json!({"router": {"listen_port": 9292}}),
        json!({"router": {"models_max": 1}}),
        json!({"aux_router": {"auto_start": true}}),
    ] {
        let (status, body) = op(&base, "settings_set_full", gone.clone()).await;
        assert_eq!(status, 400, "{gone} was accepted: {body}");
    }
}

/// Every class publishes and accepts its own `request_timeout_seconds`, and a
/// 0 survives the round trip as a 0 — it means "no ceiling", so a patch that
/// helpfully clamped it to 1 would be changing the owner's answer.
#[tokio::test]
async fn each_class_publishes_and_accepts_its_own_request_ceiling() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;

    let v = get_json(&base, "/api/settings-full").await;
    assert_eq!(v["router"]["request_timeout_seconds"], json!(600));
    assert_eq!(v["aux_router"]["request_timeout_seconds"], json!(60));
    assert_eq!(v["audio"]["request_timeout_seconds"], json!(1800));
    assert_eq!(v["image"]["request_timeout_seconds"], json!(1800));
    let dto: lmgw_api_types::SettingsFull = serde_json::from_value(v).unwrap();
    assert_eq!(dto.aux_router.request_timeout_seconds, 60);

    let (status, body) = op(
        &base,
        "settings_set_full",
        json!({
            "router": {"request_timeout_seconds": 90},
            "aux_router": {"request_timeout_seconds": 15},
            "audio": {"request_timeout_seconds": 0},
            "image": {"request_timeout_seconds": 3600},
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let snap = state.snapshot();
    assert_eq!(snap.settings.router.request_timeout_seconds, 90);
    assert_eq!(snap.settings.aux_router.request_timeout_seconds, 15);
    assert_eq!(snap.settings.audio.request_timeout_seconds, 0);
    assert_eq!(snap.settings.image.request_timeout_seconds, 3600);
    // And each reaches the upstream the requests are actually built against.
    assert_eq!(snap.router_upstream().timeout_ms, 90_000);
    assert_eq!(snap.aux_upstream().timeout_ms, 15_000);
    assert_eq!(snap.audio_upstream().request_timeout(), None);
    assert_eq!(snap.image_upstream().timeout_ms, 3_600_000);

    // It is persisted, not just held in the snapshot.
    let v = get_json(&base, "/api/settings-full").await;
    assert_eq!(v["audio"]["request_timeout_seconds"], json!(0));
    assert_eq!(v["router"]["request_timeout_seconds"], json!(90));
}

/// `container_prefix` is the first component of every container's name *and*
/// the `lmgw.instance` label reconciliation filters on (§3.3), and both are
/// fixed at `podman run` time.
///
/// So it gets two things nothing else in the patch needs: a charset check
/// (podman rejects the name hours later, on a path whose error message is
/// about container names rather than about this field), and a note when it
/// changes with containers running — those keep the old prefix, and
/// reconciliation under the new one will not even see them.
#[tokio::test]
async fn the_container_prefix_is_validated_and_a_change_says_what_it_does_not_do() {
    use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
    use std::sync::Arc;

    struct Podman;
    #[async_trait::async_trait]
    impl CommandRunner for Podman {
        async fn run(&self, _program: &str, _args: &[String]) -> std::io::Result<CmdOutput> {
            Ok(CmdOutput {
                status: 0,
                stdout: "c0ffee\n".into(),
                stderr: String::new(),
            })
        }
    }

    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;

    for bad in [
        "", "   ", "-lmgw", "lmgw-", "LMGW", "lm gw", "lmgw/dev", "lmgw_dev",
    ] {
        let (status, body) = op(&base, "settings_set_full", json!({"container_prefix": bad})).await;
        assert_eq!(status, 400, "prefix {bad:?} was accepted: {body}");
        assert_eq!(
            state.snapshot().settings.container_prefix,
            "lmgw",
            "a refused patch must not have written anything"
        );
    }

    // Nothing running: the change is fine, and the note says when it takes
    // effect rather than implying it renamed something.
    let (status, body) = op(
        &base,
        "settings_set_full",
        json!({"container_prefix": "lmgw-dev"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(state.snapshot().settings.container_prefix, "lmgw-dev");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(message.contains("prefix changed"), "{message}");

    // With a container up, the note has to say the running one is now
    // invisible to reconciliation, and what to do about it.
    let health = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&health)
        .await;
    let port = health.address().port();
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        Arc::new(Podman),
        base.client(),
        Arc::new(move || Ok(port)),
    )));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("m1.gguf"), b"not a gguf").unwrap();
    let mut s = state.snapshot().settings.clone();
    s.router.models_dir = dir.path().display().to_string();
    s.vram.load_timeout_seconds = 5;
    store::save_settings(&state.db, &s).await.unwrap();
    store::insert_local_model(&state.db, &local("m1", "m1.gguf"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    lmgw_core::ops::container(&state, None, Some("m1"), "start", false, None)
        .await
        .unwrap();
    assert_eq!(state.runtime().list().len(), 1);

    let (status, body) = op(
        &base,
        "settings_set_full",
        json!({"container_prefix": "lmgw2"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("1 model container(s) are running")
            && message.contains("reconciliation")
            && message.contains("stop the running models"),
        "the change has to name what it leaves behind: {message}"
    );
}

/// The Conversations surface lists stored conversations and its own eviction
/// rules, and the manual GC applies them.
#[tokio::test]
async fn the_responses_surface_lists_and_evicts_conversations() {
    use lmgw_core::store::StoredResponse;

    let state = AppState::init_for_tests().await.unwrap();
    for (id, chain, prev) in [
        ("r1", "r1", None),
        ("r2", "r1", Some("r1")),
        ("solo", "solo", None),
    ] {
        store::insert_response(
            &state.db,
            &StoredResponse {
                id: id.into(),
                chain_id: chain.into(),
                previous_response_id: prev.map(String::from),
                model: "my-model".into(),
                status: "completed".into(),
                body: json!({"id": id, "output": []}).to_string(),
                input_items: "[]".into(),
                messages: "[]".into(),
                pending: None,
                input_tokens: Some(3),
                output_tokens: Some(4),
                created_at: String::new(),
            },
        )
        .await
        .unwrap();
    }
    let base = serve(state.clone()).await;

    let v = get_json(&base, "/api/responses").await;
    assert_eq!(v["total_chains"], json!(2), "{v}");
    assert_eq!(v["total_responses"], json!(3), "{v}");
    let ids: Vec<&str> = v["chains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["chain_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"r1") && ids.contains(&"solo"), "{v}");
    assert!(!v["rules"].as_str().unwrap_or_default().is_empty(), "{v}");

    // The chain detail lists both of its responses.
    let v = get_json(&base, "/api/responses/chain?id=r1").await;
    let ids: Vec<&str> = v["responses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["r1", "r2"], "{v}");

    // Deleting a conversation takes the whole chain, not just one response.
    let (status, body) = op(&base, "response_chain_delete", json!({"chain_id": "r1"})).await;
    assert_eq!(status, 200, "{body}");
    for gone in ["r1", "r2"] {
        assert!(store::get_response(&state.db, gone)
            .await
            .unwrap()
            .is_none());
    }
    assert!(store::get_response(&state.db, "solo")
        .await
        .unwrap()
        .is_some());

    // And "clear all" empties the store.
    let (status, body) = op(&base, "responses_gc", json!({"scope": "all"})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(store::count_responses(&state.db).await.unwrap(), (0, 0));
}

/// Duplicating a local model clones it under a fresh `<id>-copy` id (GGUF,
/// params and args carry over) while leaving the original intact — the basis
/// for giving one GGUF several parameter presets (reasoning vs not, a
/// different slot/context split) without hand-copying every field. A second
/// duplicate avoids the `UNIQUE(model_id)` collision with a numbered suffix.
/// Restores the coverage the old `POST /local/{id}/duplicate` handler had
/// before the P8 cutover, now through `local_model_set`'s `duplicate` action.
#[tokio::test]
async fn local_duplicate_clones_under_fresh_id() {
    let state = AppState::init_for_tests().await.unwrap();
    let src = NewLocalModel {
        // A flag with no dedicated field: PROMOTED_ARGS (config/llama_params.rs) hoists
        // known flags like `--reasoning` out of `args` and into `params` on
        // every read, which would make this assertion about *args* verbatim
        // carry-over trivially true for the wrong reason.
        args: vec!["--verbose".into()],
        ..local("gemma4-12b", "gemma-4-12B.gguf")
    };
    let id = store::insert_local_model(&state.db, &src).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;

    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "duplicate", "id": id}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let models = store::list_local_models(&state.db).await.unwrap();
    let copy = models
        .iter()
        .find(|m| m.model_id == "gemma4-12b-copy")
        .expect("copy missing");
    assert!(
        models.iter().any(|m| m.model_id == "gemma4-12b"),
        "original gone"
    );
    // GGUF, args, and flags carry over verbatim — only the id differs.
    assert_eq!(copy.gguf_path, "gemma-4-12B.gguf");
    assert_eq!(copy.args, vec!["--verbose".to_string()]);
    assert!(copy.enabled && copy.public);

    // Duplicating again sidesteps the UNIQUE(model_id) collision.
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "duplicate", "id": id}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let models = store::list_local_models(&state.db).await.unwrap();
    assert!(
        models.iter().any(|m| m.model_id == "gemma4-12b-copy-2"),
        "second copy missing: {:?}",
        models.iter().map(|m| &m.model_id).collect::<Vec<_>>()
    );

    // Duplicating a model that doesn't exist is a surfaced 400, not a panic.
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "duplicate", "id": 999_999}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

/// Hiding a passthrough model removes it from `/v1/models` and the
/// client-facing catalog without deleting anything — the DB only ever holds
/// the *hidden* `(upstream_id, model_id)` set, and `server::exposed_model_names`
/// filters it in live. `/api/models/full` reports the set per upstream so the
/// UI can render a hidden row dimmed with "Unhide" rather than making it
/// silently vanish (the parity gap the P8 cutover left: `/models/{hide,unhide}`
/// had no `/api` twin and hidden rows were stuck hidden forever).
#[tokio::test]
async fn hide_and_unhide_a_passthrough_model_round_trips() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "model-a"}, {"id": "model-b"}],
        })))
        .mount(&mock)
        .await;

    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "catalog-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: mock.uri(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: true,
            expose_prefix: "cat".into(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;

    let names = |v: &Value| -> Vec<String> {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect()
    };
    let hidden_of = |v: &Value| -> Vec<String> {
        v["passthrough"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["upstream"] == json!("catalog-up"))
            .unwrap_or_else(|| panic!("catalog-up missing from passthrough: {v}"))["hidden"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h.as_str().unwrap().to_string())
            .collect()
    };

    // Both models are visible, and reported unhidden, before any hide.
    let v = get_json(&base, "/v1/models").await;
    let ns = names(&v);
    assert!(ns.contains(&"cat/model-a".to_string()), "{ns:?}");
    assert!(ns.contains(&"cat/model-b".to_string()), "{ns:?}");
    let v = get_json(&base, "/api/models/full").await;
    let pass = v["passthrough"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["upstream"] == json!("catalog-up"))
        .unwrap();
    assert_eq!(pass["id"], json!(up_id));
    assert_eq!(pass["prefix"], json!("cat"));
    assert!(hidden_of(&v).is_empty());

    // Hide model-a: gone from /v1/models, present (redacted) in the hidden set.
    let (status, body) = op(
        &base,
        "model_visibility",
        json!({"action": "hide", "upstream_id": up_id, "model_id": "model-a"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let v = get_json(&base, "/v1/models").await;
    let ns = names(&v);
    assert!(
        !ns.contains(&"cat/model-a".to_string()),
        "still exposed: {ns:?}"
    );
    assert!(ns.contains(&"cat/model-b".to_string()), "{ns:?}");
    let v = get_json(&base, "/api/models/full").await;
    assert_eq!(hidden_of(&v), vec!["model-a".to_string()]);

    // Hiding again is idempotent (INSERT OR IGNORE), not a duplicate/error.
    let (status, body) = op(
        &base,
        "model_visibility",
        json!({"action": "hide", "upstream_id": up_id, "model_id": "model-a"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = get_json(&base, "/api/models/full").await;
    assert_eq!(hidden_of(&v), vec!["model-a".to_string()]);

    // Unhide restores it.
    let (status, body) = op(
        &base,
        "model_visibility",
        json!({"action": "unhide", "upstream_id": up_id, "model_id": "model-a"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = get_json(&base, "/v1/models").await;
    assert!(names(&v).contains(&"cat/model-a".to_string()));
    let v = get_json(&base, "/api/models/full").await;
    assert!(hidden_of(&v).is_empty());

    // An unknown action is a surfaced 400, not silently ignored.
    let (status, body) = op(
        &base,
        "model_visibility",
        json!({"action": "nope", "upstream_id": up_id, "model_id": "model-a"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    // ...and a missing model_id likewise, rather than hiding an empty string.
    let (status, body) = op(
        &base,
        "model_visibility",
        json!({"action": "hide", "upstream_id": up_id}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = op(
        &base,
        "model_visibility",
        json!({"action": "hide", "upstream_id": up_id, "model_ids": ["", "  "]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // Several at once ("hide the 17 shown"): one op, both gone, both listed
    // hidden; a repeated id is counted once.
    let (status, body) = op(
        &base,
        "model_visibility",
        json!({"action": "hide", "upstream_id": up_id,
               "model_ids": ["model-a", "model-b", "model-a"]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], json!(2), "{body}");
    let ns = names(&get_json(&base, "/v1/models").await);
    assert!(
        !ns.iter().any(|n| n.starts_with("cat/")),
        "still exposed: {ns:?}"
    );
    let mut hidden = hidden_of(&get_json(&base, "/api/models/full").await);
    hidden.sort();
    assert_eq!(hidden, vec!["model-a".to_string(), "model-b".to_string()]);

    // ...and `model_id` combines with `model_ids` on the way back.
    let (status, body) = op(
        &base,
        "model_visibility",
        json!({"action": "unhide", "upstream_id": up_id,
               "model_id": "model-a", "model_ids": ["model-b"]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let ns = names(&get_json(&base, "/v1/models").await);
    assert!(ns.contains(&"cat/model-a".to_string()), "{ns:?}");
    assert!(ns.contains(&"cat/model-b".to_string()), "{ns:?}");
    assert!(hidden_of(&get_json(&base, "/api/models/full").await).is_empty());
}

/// The dashboard's catalog table reads `/api/upstream-models`, not
/// `/v1/models` (which leaves hidden models out), so the endpoint carries the
/// catalog's facts per model next to the bare ids — and only the facts the
/// catalog published: an entry that says nothing stays all-`None`. A catalog
/// that cannot be fetched is a 400 carrying the reason, not an empty list.
#[tokio::test]
async fn upstream_models_carries_each_entrys_catalog_facts() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "acme/big-coder",
                 "created": 1_784_912_544,
                 "context_length": 262_144,
                 "pricing": {"prompt": "0.0000008", "completion": "0.0000016"},
                 "top_provider": {"max_completion_tokens": 32_768},
                 "architecture": {"input_modalities": ["text", "image"],
                                  "output_modalities": ["text"]},
                 "supported_parameters": ["tools", "reasoning"]},
                {"id": "bare-model"}
            ],
        })))
        .mount(&mock)
        .await;
    let broken = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(503).set_body_string("maintenance"))
        .mount(&broken)
        .await;

    let state = AppState::init_for_tests().await.unwrap();
    let upstream = |name: &str, url: String| NewUpstream {
        name: name.into(),
        protocol: Protocol::Openai,
        kind: UpstreamKind::Generic,
        base_url: url,
        api_key: None,
        extra_headers: vec![],
        timeout_ms: 5_000,
        enabled: true,
        expose_all: true,
        expose_prefix: name.into(),
        supports_responses: false,
    };
    let good = store::insert_upstream(&state.db, &upstream("acme", mock.uri()))
        .await
        .unwrap();
    let bad = store::insert_upstream(&state.db, &upstream("down", broken.uri()))
        .await
        .unwrap();
    // A port that was free a moment ago: nothing answers there at all.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let gone = store::insert_upstream(
        &state.db,
        &upstream("gone", format!("http://127.0.0.1:{closed}/v1")),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;

    let v = get_json(&base, &format!("/api/upstream-models?id={good}")).await;
    assert_eq!(v["models"], json!(["acme/big-coder", "bare-model"]), "{v}");
    let entries = v["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 2, "{v}");
    let big = &entries[0];
    assert_eq!(big["id"], json!("acme/big-coder"));
    assert_eq!(big["context_length"], json!(262_144));
    assert_eq!(big["max_output_tokens"], json!(32_768));
    assert_eq!(big["price_prompt"], json!("0.0000008"));
    assert_eq!(big["price_completion"], json!("0.0000016"));
    assert_eq!(big["created"], json!(1_784_912_544));
    assert_eq!(big["input_modalities"], json!(["text", "image"]));
    assert_eq!(big["task"], json!("chat"));
    assert_eq!(big["tools"], json!(true));
    assert!(big["reasoning"].is_string(), "{big}");

    let bare = &entries[1];
    assert_eq!(bare["id"], json!("bare-model"));
    for k in [
        "context_length",
        "max_output_tokens",
        "price_prompt",
        "price_completion",
        "created",
        "input_modalities",
        "tools",
    ] {
        assert!(bare[k].is_null(), "{k} was invented: {bare}");
    }

    let (status, body) = get(&base, &format!("/api/upstream-models?id={bad}")).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("503"), "the reason is lost: {body}");

    // Nothing listening is its own, one-word reason, with the detail kept.
    let (status, body) = get(&base, &format!("/api/upstream-models?id={gone}")).await;
    assert_eq!(status, 400, "{body}");
    let err: Value = serde_json::from_str(&body).unwrap();
    let msg = err["message"].as_str().unwrap_or_default();
    assert!(msg.starts_with("unreachable ("), "{body}");
    assert!(
        msg.contains(&closed.to_string()),
        "the detail is lost: {body}"
    );
}

// ---------------------------------------------------------------------------
// `GET /v1/models` across all three local classes (per-model containers §5)
// ---------------------------------------------------------------------------

/// One metadata key of a hand-built GGUF header. Only the three value types
/// the capability derivation reads are modelled: the architecture/template
/// strings, the `context_length` number, and the projector's encoder flags.
pub enum Kv<'a> {
    Str(&'a str, &'a str),
    U32(&'a str, u32),
    Bool(&'a str, bool),
}

/// A GGUF file that is nothing but a header: no tensors, exactly the keys
/// given. Written to disk because that is where the exposure path reads it
/// from — the same memoized `vram::plan` footprint the scheduler sizes with,
/// not a second parser.
fn write_gguf_kvs(path: &std::path::Path, kvs: &[Kv<'_>]) {
    fn put_str(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }
    const VT_U32: u32 = 4;
    const VT_BOOL: u32 = 7;
    const VT_STRING: u32 = 8;

    let mut kv = Vec::new();
    for entry in kvs {
        match entry {
            Kv::Str(k, v) => {
                put_str(&mut kv, k);
                kv.extend_from_slice(&VT_STRING.to_le_bytes());
                put_str(&mut kv, v);
            }
            Kv::U32(k, v) => {
                put_str(&mut kv, k);
                kv.extend_from_slice(&VT_U32.to_le_bytes());
                kv.extend_from_slice(&v.to_le_bytes());
            }
            Kv::Bool(k, v) => {
                put_str(&mut kv, k);
                kv.extend_from_slice(&VT_BOOL.to_le_bytes());
                kv.push(u8::from(*v));
            }
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"GGUF");
    out.extend_from_slice(&3u32.to_le_bytes()); // version
    out.extend_from_slice(&0u64.to_le_bytes()); // tensor count
    out.extend_from_slice(&(kvs.len() as u64).to_le_bytes());
    out.extend_from_slice(&kv);
    std::fs::write(path, out).unwrap();
}

/// The smallest GGUF the header reader accepts, carrying exactly the two keys
/// `context_length` comes from.
fn write_gguf(path: &std::path::Path, arch: &str, ctx: u32) {
    let ctx_key = format!("{arch}.context_length");
    write_gguf_kvs(
        path,
        &[
            Kv::Str("general.architecture", arch),
            Kv::U32(&ctx_key, ctx),
        ],
    );
}

fn entry<'a>(v: &'a Value, id: &str) -> &'a Value {
    let hits: Vec<&Value> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["id"].as_str() == Some(id))
        .collect();
    assert_eq!(hits.len(), 1, "'{id}' should be listed exactly once: {v}");
    hits[0]
}

/// The real Qwen3.8 chat template — the same fixture `gguf::TemplateSignals`
/// and `capabilities::for_local_row` are tested against, here pushed through
/// the whole stack: written into a GGUF on disk, read back by the listing
/// handler's cache, and published as effort levels on the wire.
const QWEN38_TEMPLATE: &str = include_str!("../fixtures/chat_templates/qwen3.8.jinja");

/// `/v1/models` lists every enabled local model of all three classes, exactly
/// once each, beside aliases and the passthrough catalogs — and each one
/// carries the capability contract of its class (model capabilities design
/// §2.1, §3, §4).
///
/// Aux and audio used to arrive through the expose-all HTTP fan-out against
/// the class routers' catalogs. Per-model containers deleted those upstream
/// rows and those always-on ports, so the tables are the catalog now (§5) —
/// which this proves by seeding no `upstreams` row for either class and still
/// finding both, with `owned_by` unchanged from what the fan-out published.
#[tokio::test]
async fn v1_models_lists_all_three_local_classes_table_driven() {
    let dir = tempfile::tempdir().unwrap();
    write_gguf(&dir.path().join("bge-m3.gguf"), "bert", 512);
    // The chat row's weights: a real chat template, so the reasoning/tool
    // facts below are derived from the file rather than from the row.
    write_gguf_kvs(
        &dir.path().join("qwen3.8-27b.gguf"),
        &[
            Kv::Str("general.architecture", "qwen3"),
            Kv::U32("qwen3.context_length", 262_144),
            Kv::Str("tokenizer.chat_template", QWEN38_TEMPLATE),
        ],
    );
    // …and its projector, declaring both encoders — modalities come from this
    // header and nowhere else (§3.3).
    write_gguf_kvs(
        &dir.path().join("mmproj-qwen3.8.gguf"),
        &[
            Kv::Str("general.architecture", "clip"),
            Kv::Str("general.type", "mmproj"),
            Kv::Bool("clip.has_vision_encoder", true),
            Kv::Bool("clip.has_audio_encoder", true),
        ],
    );

    let state = AppState::init_for_tests().await.unwrap();

    // A real passthrough upstream stays in the picture: catalog.rs is still
    // the path for those, and nothing may be listed twice because of it. Its
    // catalog is Kilo/OpenRouter-shaped, which is where a cloud model's
    // modalities, effort ladder and output cap come from (§4).
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "gpt-9",
                "context_length": 200_000,
                "architecture": {
                    "input_modalities": ["text", "image"],
                    "output_modalities": ["text"],
                },
                "supported_parameters": [
                    "tools", "reasoning", "reasoning_effort", "response_format",
                    "structured_outputs",
                ],
                "top_provider": {"max_completion_tokens": 64_000},
                "opencode": {"variants": {
                    "none": {}, "low": {}, "medium": {}, "high": {},
                }},
            }]
        })))
        .mount(&mock)
        .await;
    store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "cloud".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: true,
            expose_prefix: "cloud".into(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();

    let mut settings = state.snapshot().settings.clone();
    settings.aux_router.models_dir = dir.path().display().to_string();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();

    let mut chat = local("qwen3.8-27b", "qwen3.8-27b.gguf");
    chat.params.ctx_size = Some(16_384);
    chat.params.parallel = Some(2);
    chat.params.n_predict = Some(32_768);
    chat.params.reasoning = Some("on".into());
    chat.params.mmproj_path = Some("mmproj-qwen3.8.gguf".into());
    store::insert_local_model(&state.db, &chat).await.unwrap();
    let mut private = local("private", "private.gguf");
    private.public = false;
    store::insert_local_model(&state.db, &private)
        .await
        .unwrap();

    for (model_id, enabled) in [("bge-m3", true), ("bge-reranker", false)] {
        store::insert_aux_model(
            &state.db,
            &NewAuxModel {
                model_id: model_id.into(),
                gguf_path: format!("{model_id}.gguf"),
                kind: AuxKind::Embed,
                pooling: None,
                ctx_size: None,
                args: vec![],
                idle_seconds: 0,
                enabled,
                image: None,
                extra_run_args: None,
                warm_start: false,
                hold_fallback_mode: Default::default(),
                hold_fallback: None,
            },
        )
        .await
        .unwrap();
    }
    for (model_id, enabled) in [("pocket-tts", true), ("retired-tts", false)] {
        store::insert_audio_model(
            &state.db,
            &NewAudioModel {
                model_id: model_id.into(),
                family: "pocket_tts".into(),
                path: format!("audio/{model_id}"),
                task: "tts".into(),
                mode: "offline".into(),
                lazy: None,
                busy_timeout_ms: None,
                load_options: Default::default(),
                session_options: Default::default(),
                default_request_options: Default::default(),
                model_spec_override: None,
                config_id: None,
                weight_id: None,
                voice_presets: Default::default(),
                default_voice_preset: None,
                enabled,
                image: None,
                extra_run_args: None,
                warm_start: false,
                hold_fallback_mode: Default::default(),
                hold_fallback: None,
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;

    let v = get_json(&base, "/v1/models").await;

    // Chat: the configured ctx-size divided by the parallel slots one request
    // can actually use — unchanged.
    let chat = entry(&v, "qwen3.8-27b");
    assert_eq!(chat["object"].as_str(), Some("model"));
    assert_eq!(chat["owned_by"].as_str(), Some("llama-server"));
    assert_eq!(chat["context_length"].as_u64(), Some(8_192));
    assert_eq!(chat["pricing"]["prompt"].as_str(), Some("0"));

    // …and the capability contract derived from its own files (§3.2–§3.4):
    // the template's effort ladder, the projector's modalities, the row's
    // `--reasoning on` and `--n-predict`.
    let caps = &chat["capabilities"];
    assert_eq!(caps["task"].as_str(), Some("chat"), "{chat}");
    assert_eq!(caps["source"].as_str(), Some("gguf+config"), "{chat}");
    assert_eq!(caps["reasoning"]["kind"].as_str(), Some("levels"), "{chat}");
    assert_eq!(
        caps["reasoning"]["levels"],
        json!(["low", "medium", "high", "xhigh"]),
        "{chat}"
    );
    assert_eq!(caps["reasoning"]["enabled"].as_bool(), Some(true), "{chat}");
    assert_eq!(
        caps["reasoning"]["can_disable"].as_bool(),
        Some(true),
        "{chat}"
    );
    assert_eq!(
        caps["input_modalities"],
        json!(["text", "image", "audio"]),
        "{chat}"
    );
    assert_eq!(caps["vision"].as_bool(), Some(true), "{chat}");
    assert_eq!(
        caps["tool_calls"]["format"].as_str(),
        Some("qwen-xml"),
        "{chat}"
    );
    assert_eq!(chat["max_output_tokens"].as_u64(), Some(32_768), "{chat}");
    assert!(
        !chat["notes"].as_array().unwrap().is_empty(),
        "a local chat row must explain itself: {chat}"
    );

    // Aux: listed from the table, owned by the synthetic upstream that kept
    // the managed row's name, with the context window read out of the GGUF.
    let aux = entry(&v, "embed/bge-m3");
    assert_eq!(aux["object"].as_str(), Some("model"));
    assert_eq!(aux["owned_by"].as_str(), Some("llama-aux"));
    assert_eq!(aux["context_length"].as_u64(), Some(512));
    assert_eq!(aux["pricing"]["completion"].as_str(), Some("0"));
    assert_eq!(aux["capabilities"]["task"].as_str(), Some("embedding"));
    assert_eq!(
        aux["capabilities"]["output_modalities"],
        json!(["embedding"])
    );
    assert_eq!(aux["capabilities"]["endpoints"], json!(["/v1/embeddings"]));
    // An embedder has no output cap to publish and nothing to reason with.
    assert!(aux.get("max_output_tokens").is_none(), "{aux}");

    // Audio: audio.cpp publishes no model metadata at all, so there is no
    // context window — omitted rather than invented.
    let audio = entry(&v, "audio/pocket-tts");
    assert_eq!(audio["object"].as_str(), Some("model"));
    assert_eq!(audio["owned_by"].as_str(), Some("audiocpp"));
    assert!(audio.get("context_length").is_none(), "{audio}");
    assert_eq!(audio["pricing"]["prompt"].as_str(), Some("0"));
    // The row's `--task` is the capability: tts speaks text in, audio out, on
    // the two OpenAI-shaped speech routes.
    assert_eq!(audio["capabilities"]["task"].as_str(), Some("tts"));
    assert_eq!(audio["capabilities"]["input_modalities"], json!(["text"]));
    assert_eq!(audio["capabilities"]["output_modalities"], json!(["audio"]));
    assert_eq!(
        audio["capabilities"]["endpoints"],
        json!(["/v1/audio/speech", "/v1/audio/voices"])
    );

    // The passthrough catalog still fans out beside them — and publishes
    // exactly what the provider stated, no more (§4).
    let cloud = entry(&v, "cloud/gpt-9");
    assert_eq!(cloud["owned_by"].as_str(), Some("cloud"));
    assert_eq!(cloud["capabilities"]["source"].as_str(), Some("catalog"));
    assert_eq!(
        cloud["capabilities"]["input_modalities"],
        json!(["text", "image"])
    );
    assert_eq!(
        cloud["capabilities"]["reasoning"]["levels"],
        json!(["low", "medium", "high"]),
        "{cloud}"
    );
    assert_eq!(
        cloud["capabilities"]["reasoning"]["can_disable"].as_bool(),
        Some(true),
        "{cloud}"
    );
    assert_eq!(cloud["max_output_tokens"].as_u64(), Some(64_000), "{cloud}");
    // A catalog states no default state for reasoning, so the field is absent
    // rather than guessed.
    assert!(
        cloud["capabilities"]["reasoning"].get("enabled").is_none(),
        "{cloud}"
    );

    // Disabled rows and a non-public chat model are not client-routable.
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    for hidden in ["embed/bge-reranker", "audio/retired-tts", "private"] {
        assert!(
            !ids.contains(&hidden),
            "{hidden} should not be listed: {ids:?}"
        );
    }

    // And each of them really resolves to its own class's synthetic upstream.
    let snap = state.snapshot();
    assert_eq!(
        snap.resolve("embed/bge-m3").unwrap().upstream.name,
        "llama-aux"
    );
    assert_eq!(
        snap.resolve("audio/pocket-tts").unwrap().upstream.name,
        "audiocpp"
    );
}

// ---------------------------------------------------------------------------
// GPU hold — per-model and global fallback (gpu-hold design §2/§3.2/§3.3),
// work package 1: model + plumbing only, no request routing yet.
// ---------------------------------------------------------------------------

/// A cloud alias that resolves and classifies as non-local — a "usable"
/// fallback (§2) — without needing a live upstream: `validate_fallback_alias`
/// only resolves and classifies, it never sends a request.
async fn insert_cloud_alias(state: &SharedState, alias: &str) {
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: format!("{alias}-up"),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: "http://127.0.0.1:1/v1".into(),
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
            alias: alias.into(),
            upstream_id: up_id,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// `hold_fallback_mode = "alias"` + `hold_fallback` round-trips through the
/// store and the editor's read endpoint; switching to `none` drops a
/// previously-set alias; `clear: "hold_fallback"` resets both columns to
/// inherit/NULL.
#[tokio::test]
async fn local_model_set_hold_fallback_round_trips_and_clears() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::insert_local_model(&state.db, &local("qwen3", "qwen3.gguf"))
        .await
        .unwrap();
    insert_cloud_alias(&state, "cloud-fallback").await;
    let base = serve(state.clone()).await;

    let saved = || async {
        store::list_local_models(&state.db)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.model_id == "qwen3")
            .unwrap()
    };

    let (status, body) = op(
        &base,
        "local_model_set",
        json!({
            "action": "update", "id": id,
            "hold_fallback_mode": "alias",
            "hold_fallback": "cloud-fallback",
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let m = saved().await;
    assert_eq!(m.hold_fallback_mode, HoldFallbackMode::Alias);
    assert_eq!(m.hold_fallback.as_deref(), Some("cloud-fallback"));

    // The editor's read endpoint hands both fields back.
    let v = get_json(&base, &format!("/api/local-model?id={id}")).await;
    assert_eq!(v["hold_fallback_mode"], json!("alias"));
    assert_eq!(v["hold_fallback"], json!("cloud-fallback"));

    // Switching to `none` drops the now-meaningless alias.
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "update", "id": id, "hold_fallback_mode": "none"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let m = saved().await;
    assert_eq!(m.hold_fallback_mode, HoldFallbackMode::None);
    assert_eq!(m.hold_fallback, None);

    // Back to `alias`, then `clear: "hold_fallback"` resets both to inherit/NULL
    // — the same convention `image`/`extra_run_args` already use.
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({
            "action": "update", "id": id,
            "hold_fallback_mode": "alias",
            "hold_fallback": "cloud-fallback",
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "update", "id": id, "clear": "hold_fallback"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let m = saved().await;
    assert_eq!(m.hold_fallback_mode, HoldFallbackMode::Inherit);
    assert_eq!(m.hold_fallback, None);
}

/// A fallback alias must resolve to a non-local route (§2/§3.3): one that is
/// itself a local model is refused, naming why.
#[tokio::test]
async fn local_model_set_refuses_a_hold_fallback_that_is_itself_local() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::insert_local_model(&state.db, &local("qwen3", "qwen3.gguf"))
        .await
        .unwrap();
    store::insert_local_model(&state.db, &local("other-local", "other.gguf"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;

    let (status, body) = op(
        &base,
        "local_model_set",
        json!({
            "action": "update", "id": id,
            "hold_fallback_mode": "alias",
            "hold_fallback": "other-local",
        }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("local"),
        "{body}"
    );

    // An alias that does not resolve at all is refused the same way.
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({
            "action": "update", "id": id,
            "hold_fallback_mode": "alias",
            "hold_fallback": "no-such-alias",
        }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

/// The global `hold.fallback_alias` round-trips through `GET
/// /api/settings-full`, validates the same way the per-model one does, `""`
/// clears it, and `active` is not reachable through this generic patch (it is
/// `ops::hold_set`'s job, per §3.1 — package 2).
#[tokio::test]
async fn settings_hold_fallback_alias_round_trips_validates_and_clears() {
    let state = AppState::init_for_tests().await.unwrap();
    insert_cloud_alias(&state, "cloud-fallback").await;
    let base = serve(state.clone()).await;

    let (status, body) = op(
        &base,
        "settings_set_full",
        json!({"hold": {"fallback_alias": "cloud-fallback"}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let v = get_json(&base, "/api/settings-full").await;
    assert_eq!(v["hold"]["fallback_alias"], json!("cloud-fallback"));
    assert_eq!(v["hold"]["active"], json!(false));
    let dto: lmgw_api_types::SettingsFull = serde_json::from_value(v).unwrap();
    assert_eq!(dto.hold.fallback_alias.as_deref(), Some("cloud-fallback"));
    assert!(!dto.hold.active);

    // `""` clears it back to "refuse".
    let (status, body) = op(
        &base,
        "settings_set_full",
        json!({"hold": {"fallback_alias": ""}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = get_json(&base, "/api/settings-full").await;
    assert_eq!(v["hold"]["fallback_alias"], Value::Null);

    // Unknown alias: refused, not silently stored.
    let (status, body) = op(
        &base,
        "settings_set_full",
        json!({"hold": {"fallback_alias": "no-such-alias"}}),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // `active` has no field on this patch at all.
    let (status, body) = op(
        &base,
        "settings_set_full",
        json!({"hold": {"active": true}}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

/// The one `settings_hold_fallback_alias_round_trips_validates_and_clears`
/// does not cover: a global fallback that resolves, but to a *local* model —
/// refused the same way the per-model patch already is.
#[tokio::test]
async fn settings_set_full_refuses_a_global_fallback_that_is_itself_local() {
    let state = AppState::init_for_tests().await.unwrap();
    store::insert_local_model(&state.db, &local("qwen3", "qwen3.gguf"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;

    let (status, body) = op(
        &base,
        "settings_set_full",
        json!({"hold": {"fallback_alias": "qwen3"}}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("local"),
        "{body}"
    );
}

/// `hold_fallback_mode = "alias"` without an alias to route to is refused, not
/// silently stored as a mode with nothing behind it.
#[tokio::test]
async fn local_model_set_refuses_alias_mode_with_no_alias() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::insert_local_model(&state.db, &local("qwen3", "qwen3.gguf"))
        .await
        .unwrap();
    let base = serve(state.clone()).await;

    let (status, body) = op(
        &base,
        "local_model_set",
        json!({"action": "update", "id": id, "hold_fallback_mode": "alias"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("requires hold_fallback"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// GPU hold — the op (gpu-hold design §6/§7.10)
// ---------------------------------------------------------------------------

/// `POST /api/op/hold_set` round trips both directions over the real router,
/// and a missing or non-bool `active` is a readable 400, not a panic or a
/// silent no-op.
#[tokio::test]
async fn hold_set_op_round_trips_and_a_bad_active_is_a_readable_400() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;

    let (status, body) = op(&base, "hold_set", json!({"active": true})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["active"], true, "{body}");
    let v = get_json(&base, "/api/settings-full").await;
    assert_eq!(v["hold"]["active"], true, "{v}");

    let (status, body) = op(&base, "hold_set", json!({"active": false})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["active"], false, "{body}");
    let v = get_json(&base, "/api/settings-full").await;
    assert_eq!(v["hold"]["active"], false, "{v}");

    let (status, body) = op(&base, "hold_set", json!({})).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("pass active: true or false"),
        "{body}"
    );

    let (status, body) = op(&base, "hold_set", json!({"active": "yes"})).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("pass active: true or false"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// Owner overrides (model-capabilities design §7)
// ---------------------------------------------------------------------------

/// `capabilities_override` round-trips through the two dashboard forms that
/// actually edit it: the local model editor
/// (`POST /api/op/local_model_set` + `GET /api/local-model`) and the alias
/// editor (`POST /api/op/alias_set` + `GET /api/models/full`, since
/// `alias_set` — not the MCP-parity `model_set` — is what
/// `crates/lmgw-ui/src/pages/model_editors.rs`'s form actually calls).
#[tokio::test]
async fn capabilities_override_round_trips_through_the_dashboard_local_and_alias_forms() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;

    // -- local model editor --
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({ "action": "create", "model_id": "owner-caps", "gguf_path": "a.gguf" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let id = body["id"].as_i64().unwrap();

    let (status, body) = op(
        &base,
        "local_model_set",
        json!({
            "action": "update", "id": id,
            "capabilities_override": r#"{"max_output_tokens":8192,"notes":["hand-verified"]}"#,
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let v = get_json(&base, &format!("/api/local-model?id={id}")).await;
    assert_eq!(
        v["capabilities_override"],
        json!({"max_output_tokens": 8192, "notes": ["hand-verified"]}),
        "{v}"
    );

    // A malformed override is refused, and the stored value survives.
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({ "action": "update", "id": id, "capabilities_override": "{\"capabilities\": 1}" }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let v = get_json(&base, &format!("/api/local-model?id={id}")).await;
    assert_eq!(
        v["capabilities_override"],
        json!({"max_output_tokens": 8192, "notes": ["hand-verified"]}),
        "a rejected edit overwrote the stored override: {v}"
    );

    // Clearing (the form's empty-textarea convention: name it in `clear`).
    let (status, body) = op(
        &base,
        "local_model_set",
        json!({ "action": "update", "id": id, "clear": "capabilities_override" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = get_json(&base, &format!("/api/local-model?id={id}")).await;
    assert!(v["capabilities_override"].is_null(), "{v}");

    // -- alias editor --
    let (status, up_body) = op(
        &base,
        "upstream_set",
        json!({
            "action": "create", "name": "cloud-caps", "protocol": "openai",
            "base_url": "http://127.0.0.1:1/v1",
        }),
    )
    .await;
    assert_eq!(status, 200, "{up_body}");
    let up_id = up_body["id"].as_i64().unwrap();

    let (status, alias_body) = op(
        &base,
        "alias_set",
        json!({
            "action": "create", "alias": "caps-alias",
            "upstream_id": up_id, "upstream_model": "tgt",
            "capabilities_override": json!({
                "capabilities": {
                    "task": "chat",
                    "endpoints": ["/v1/chat/completions"],
                    "source": "catalog",
                }
            }).to_string(),
        }),
    )
    .await;
    assert_eq!(status, 200, "{alias_body}");
    let alias_id = alias_body["id"].as_i64().unwrap();

    let find_alias = |v: &Value| -> Value {
        v["aliases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["alias"] == "caps-alias")
            .unwrap()
            .clone()
    };
    let v = get_json(&base, "/api/models/full").await;
    assert_eq!(
        find_alias(&v)["capabilities_override"],
        json!({"capabilities": {
            "task": "chat", "endpoints": ["/v1/chat/completions"], "source": "catalog",
        }}),
        "{v}"
    );

    // Sent wholesale like `overrides` — an empty string clears (same
    // convention `chat_template_kwargs` uses).
    let (status, body) = op(
        &base,
        "alias_set",
        json!({ "action": "update", "id": alias_id, "capabilities_override": "" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = get_json(&base, "/api/models/full").await;
    assert!(find_alias(&v)["capabilities_override"].is_null(), "{v}");
}

// ---------------------------------------------------------------------------
// The image class on the dashboard plane (image-generation design §8, WP4)
// ---------------------------------------------------------------------------

/// `POST /api/op/image_model_set` is the *same function*
/// `lmgw__image_model_set` calls — the design's one deliberate departure from
/// the audio precedent, whose CRUD lives in the web layer alone. So this
/// asserts the dashboard's shape of the call: a real JSON object for the two
/// maps (the tool plane sends `key = value` lines, because MCP schemas are
/// flat scalars), and the row and its rendered command line coming back.
#[tokio::test]
async fn image_model_set_round_trips_through_the_dashboard_op_plane() {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("leejet/z")).unwrap();
    std::fs::write(dir.path().join("leejet/z/weights.gguf"), b"w").unwrap();
    std::fs::write(dir.path().join("leejet/z/ae.safetensors"), b"v").unwrap();
    let base = serve(state.clone()).await;

    let (status, body) = op(
        &base,
        "settings_set_full",
        json!({"image": {"models_dir": dir.path().display().to_string()}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = op(
        &base,
        "image_model_set",
        json!({
            "action": "create",
            "model_id": "z-image",
            "files": {
                "diffusion_model": "leejet/z/weights.gguf",
                "vae": "leejet/z/ae.safetensors",
            },
            "args": { "diffusion_fa": true, "steps": 8 },
            "modes": ["img_gen"],
            "edit": true,
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["public_name"], "image/z-image");
    let cmd = body["command_line"].as_str().unwrap();
    assert!(
        cmd.contains("--diffusion-model /models/leejet/z/weights.gguf")
            && cmd.contains("--eager-load"),
        "{cmd}"
    );

    // …and it is on the models payload the Models page reads, under the
    // section the UI deserializes.
    let v = get_json(&base, "/api/models/full").await;
    assert_eq!(v["image"][0]["public_name"], "image/z-image");
    let dto: lmgw_api_types::ModelsFull = serde_json::from_value(v).unwrap();
    assert_eq!(dto.image.len(), 1);
    assert_eq!(dto.image[0].model.model_id, "z-image");
    assert!(dto.image[0].model.edit);
    assert_eq!(dto.image[0].model.modes, vec!["img_gen".to_string()]);
    assert_eq!(
        dto.image[0].model.files["diffusion_model"],
        json!("leejet/z/weights.gguf")
    );
    assert_eq!(dto.image[0].model.args["steps"], json!(8));

    // A refusal is a 400 with the sentence, not a stored row.
    let (status, body) = op(
        &base,
        "image_model_set",
        json!({
            "action": "update", "model_id": "z-image",
            "files": {"diffusion_model": "leejet/z/gone.gguf"},
        }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(state.snapshot().image_models.len(), 1);
    assert_eq!(
        state.snapshot().image_models[0].files["diffusion_model"],
        json!("leejet/z/weights.gguf")
    );

    let (status, body) = op(
        &base,
        "image_model_set",
        json!({"action": "delete", "model_id": "z-image"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(state.snapshot().image_models.is_empty());
}

/// The settings DTO gains an `image` section — a contract change to any
/// third-party caller of `/api/settings-full`, since the patch is
/// `deny_unknown_fields` on both ends.
///
/// Naming `models_dir` also creates the two directories sd-server's
/// capabilities route throws without (§3, §12.2). They are made again before
/// every start, which covers one deleted in between; this is what makes them
/// exist for an owner who looks at the tree before starting anything.
#[tokio::test]
async fn the_settings_plane_carries_the_image_class_and_makes_its_two_directories() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;

    let v = get_json(&base, "/api/settings-full").await;
    for key in ["image", "models_dir", "extra_run_args", "public_prefix"] {
        assert!(v["image"].get(key).is_some(), "image.{key} missing: {v}");
    }
    // No engine fields: sd-server has no config file, so anything per-process
    // is a flag in some row's args.
    for gone in ["backend", "device", "threads", "lazy_load"] {
        assert!(v["image"].get(gone).is_none(), "image.{gone} exists: {v}");
    }
    let dto: lmgw_api_types::SettingsFull = serde_json::from_value(v).unwrap();
    assert_eq!(
        dto.image.image,
        "ghcr.io/leejet/stable-diffusion.cpp:master-cuda"
    );
    assert_eq!(dto.image.public_prefix, "image");
    assert!(dto.image.models_dir.is_empty());

    let dir = tempfile::tempdir().unwrap();
    let (status, body) = op(
        &base,
        "settings_set_full",
        json!({"image": {
            "models_dir": dir.path().display().to_string(),
            "public_prefix": "/pictures/",
        }}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let s = state.snapshot().settings.image.clone();
    assert_eq!(s.models_dir, dir.path().display().to_string());
    assert_eq!(s.public_prefix, "pictures");
    assert!(dir.path().join("loras").is_dir(), "loras/ was not created");
    assert!(
        dir.path().join("upscalers").is_dir(),
        "upscalers/ was not created"
    );

    // Closed patch: a key that does not exist is an error, never dropped.
    let (status, body) = op(
        &base,
        "settings_set_full",
        json!({"image": {"backend": "cuda"}}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

/// The dashboard's HTML carries the narrow CSP that keeps replies from loading
/// remote images; API responses and non-HTML assets do not.
#[tokio::test]
async fn the_dashboard_html_carries_the_narrow_csp_and_the_api_does_not() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state).await;
    let csp = |path: &'static str| {
        let base = &base;
        async move {
            let r = base
                .client()
                .get(format!("{base}{path}"))
                .send()
                .await
                .unwrap();
            (
                r.status().as_u16(),
                r.headers()
                    .get("content-security-policy")
                    .map(|v| v.to_str().unwrap().to_string()),
            )
        }
    };
    let want = "img-src 'self' data: blob:; media-src 'self' data: blob:; font-src 'self' data:; style-src 'self' 'unsafe-inline'";
    for path in ["/", "/chat", "/agents/docs-librarian"] {
        let (status, got) = csp(path).await;
        assert_eq!(status, 200, "{path}");
        assert_eq!(got.as_deref(), Some(want), "{path}");
    }
    let (_, got) = csp("/v1/models").await;
    assert_eq!(got, None, "/v1/models");
    let (_, got) = csp("/vendor/highlight.js").await;
    assert_eq!(got, None, "js asset");
}
