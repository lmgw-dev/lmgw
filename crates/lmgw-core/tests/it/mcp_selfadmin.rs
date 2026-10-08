//! Built-in self-admin tools (§20) end-to-end over the real northbound
//! `/mcp/admin`.
//!
//! These drive the same wire an agent does — `initialize`, `tools/list`,
//! `tools/call` — because the parts worth protecting are the *seams*: the mode
//! gate, the reserved namespace, the audit row, and whether a mutation actually
//! reaches the database and the live snapshot.
//!
//! Since principals §3.7 the plane lives on its own route behind an **owner
//! credential** — the `owner:self-admin` key — so the harness installs one
//! with a known plaintext and presents it. That the route refuses a disabled
//! row, and that the tools are gone from `/mcp`, is covered in
//! `mcp_admin_plane.rs`.

use lmgw_core::config::{SelfAdmin, Settings};
use lmgw_core::gguf::synth;
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn serve_with(state: SharedState) -> String {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// The credential these tests present: the plaintext of the enabled
/// `owner:self-admin` key the harness installs. Any value works; what matters
/// is that the route admits an owner key and nothing else.
const ADMIN_TOKEN: &str = "test-admin-token";

/// A gateway with `self_admin` persisted at `mode`. Written through the store
/// (not `set_snapshot_for_tests`) because every mutating tool calls
/// `reload_snapshot()`, which would discard a hand-swapped snapshot.
async fn state_with_mode(mode: SelfAdmin) -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let settings = Settings {
        self_admin: mode,
        ..Settings::default()
    };
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    lmgw_core::agents::token::set_owner_key(
        &state,
        lmgw_core::agents::token::OWNER_SELF_ADMIN,
        ADMIN_TOKEN,
        true,
    )
    .await
    .unwrap();
    state
}

async fn post(base: &str, session: Option<&str>, body: Value) -> (u16, Option<String>, Value) {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/mcp/admin"))
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {ADMIN_TOKEN}"))
        .json(&body);
    if let Some(sid) = session {
        req = req.header("mcp-session-id", sid);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let text = resp.text().await.unwrap_or_default();
    (
        status,
        sid,
        serde_json::from_str(&text).unwrap_or(Value::Null),
    )
}

async fn initialize(base: &str) -> String {
    let (status, sid, body) = post(
        base,
        None,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                        "clientInfo": { "name": "selfadmin-test", "version": "0" } }
        }),
    )
    .await;
    assert_eq!(status, 200, "initialize should 200: {body}");
    sid.expect("initialize must return a session id")
}

async fn list_tool_names(base: &str, sid: &str) -> Vec<String> {
    list_tools(base, sid)
        .await
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// [`list_tool_names`], keeping each tool's full `tools/list` entry
/// (`inputSchema` included) for a test that inspects a schema rather than
/// just checking a name is present.
async fn list_tools(base: &str, sid: &str) -> Vec<Value> {
    let (status, _, body) = post(
        base,
        Some(sid),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    assert_eq!(status, 200);
    body["result"]["tools"].as_array().unwrap().clone()
}

fn tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("{name} missing from tools/list"))
}

/// Call a tool and return its `result` object (JSON-RPC errors are asserted
/// away — a `-32601` here means the dispatcher, not the tool, refused).
async fn call(base: &str, sid: &str, name: &str, args: Value) -> Value {
    let (status, _, body) = post(
        base,
        Some(sid),
        json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        }),
    )
    .await;
    assert_eq!(status, 200, "tools/call should 200: {body}");
    assert!(
        body["error"].is_null(),
        "unexpected JSON-RPC error for {name}: {body}"
    );
    body["result"].clone()
}

fn is_error(result: &Value) -> bool {
    result["isError"] == Value::Bool(true)
}

fn text_of(result: &Value) -> String {
    result["content"][0]["text"].as_str().unwrap().to_string()
}

/// Parse a successful tool result's JSON payload back out of its text block.
fn payload(result: &Value) -> Value {
    assert!(!is_error(result), "tool failed: {}", text_of(result));
    serde_json::from_str(&text_of(result)).expect("tool payload should be JSON")
}

// ---------------------------------------------------------------------------
// The mode gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_only_lists_reads_and_hides_mutations() {
    let base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let sid = initialize(&base).await;
    let names = list_tool_names(&base, &sid).await;

    for expected in [
        "lmgw__status",
        "lmgw__models",
        "lmgw__upstreams",
        "lmgw__mcp_servers",
        "lmgw__logs",
        "lmgw__settings",
    ] {
        assert!(names.iter().any(|n| n == expected), "missing {expected}");
    }
    for forbidden in [
        "lmgw__upstream_set",
        "lmgw__model_set",
        "lmgw__local_model_set",
        "lmgw__mcp_server_set",
        "lmgw__container",
        "lmgw__settings_set",
    ] {
        assert!(
            !names.iter().any(|n| n == forbidden),
            "{forbidden} must not be listed at read_only"
        );
    }
}

#[tokio::test]
async fn read_only_refuses_a_mutation_with_an_actionable_error() {
    let base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let sid = initialize(&base).await;

    let result = call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "delete", "id": 1 }),
    )
    .await;

    // A refusal is a tool error the model can read and act on, not a protocol
    // error — and it must name the setting that would allow the call.
    assert!(is_error(&result));
    let msg = text_of(&result);
    assert!(msg.contains("read_only"), "unhelpful refusal: {msg}");
    assert!(msg.contains("full"), "refusal should name the fix: {msg}");
}

#[tokio::test]
async fn off_exposes_nothing_but_still_explains_itself() {
    let base = serve_with(state_with_mode(SelfAdmin::Off).await).await;
    let sid = initialize(&base).await;

    assert!(
        list_tool_names(&base, &sid).await.is_empty(),
        "self_admin=off must expose no tools at all"
    );

    // Reserved names never fall through to the aggregate, so a call while off
    // reports the disabled feature rather than a misleading "unknown tool".
    let result = call(&base, &sid, "lmgw__status", json!({})).await;
    assert!(is_error(&result));
    assert!(
        text_of(&result).contains("disabled"),
        "{}",
        text_of(&result)
    );
}

#[tokio::test]
async fn full_lists_every_tool() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;
    let names = list_tool_names(&base, &sid).await;

    assert!(names.iter().all(|n| n.starts_with("lmgw__")));
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "duplicate tool name: {names:?}");

    // Named rather than counted: a count assertion has to be edited every time
    // a tool is added, which makes it a chore instead of a check. These are
    // the tools the documented add-a-model path depends on, so their absence
    // is a real regression.
    for required in [
        "lmgw__status",
        "lmgw__hf_repo",
        "lmgw__hf_add",
        "lmgw__hf_downloads",
        "lmgw__gguf_files",
        "lmgw__model_inspect",
        "lmgw__local_model_plan",
        "lmgw__local_model_set",
        "lmgw__local_model_get",
        "lmgw__local_model_test",
        "lmgw__container",
    ] {
        assert!(names.iter().any(|n| n == required), "missing {required}");
    }

    // Everything visible at read_only must still be visible at full.
    let ro_base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let ro_sid = initialize(&ro_base).await;
    for n in list_tool_names(&ro_base, &ro_sid).await {
        assert!(names.contains(&n), "{n} is readable but missing at full");
    }
}

/// The ladder design (§6, §8 WP6): `local_model_set` gains a `ladder`
/// property and `clear` names it, `local_model_get`/`local_model_test`
/// describe the per-rung behaviour, and `status`/`logs` name the rung.
/// `ladder` stays a flat string like every other argument in this module
/// (`all_parameters_are_flat_scalars` pins that for the whole catalog) —
/// `hoist_ladder_arg` is what turns it into the real array before
/// `patch_from_args` ever sees it.
#[tokio::test]
async fn local_model_set_schema_describes_ladder_as_a_json_string() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;
    let tools = list_tools(&base, &sid).await;

    let set = tool(&tools, "lmgw__local_model_set");
    let ladder = &set["inputSchema"]["properties"]["ladder"];
    assert_eq!(ladder["type"], "string", "{ladder}");
    let desc = ladder["description"].as_str().unwrap();
    assert!(desc.contains("gguf_path"), "{desc}");
    assert!(desc.contains("ctx_size"), "{desc}");

    let clear = set["inputSchema"]["properties"]["clear"]["description"]
        .as_str()
        .unwrap();
    assert!(clear.contains("ladder"), "{clear}");

    let get = tool(&tools, "lmgw__local_model_get");
    assert!(get["description"].as_str().unwrap().contains("rungs"));

    let test = tool(&tools, "lmgw__local_model_test");
    assert!(test["description"].as_str().unwrap().contains("rung"));

    let status = tool(&tools, "lmgw__status");
    assert!(status["description"].as_str().unwrap().contains("climbing"));

    let logs = tool(&tools, "lmgw__logs");
    assert!(logs["description"].as_str().unwrap().contains("rung"));
}

/// The ladder plumbing over the real wire, the schema's own JSON-string
/// shape: `hoist_ladder_arg` parses the tool call's raw `ladder` string
/// argument into the real array before `patch_from_args`'s generic
/// deserialize turns it into `Vec<Rung>` (`ladder_models.rs` covers
/// `ops::local_model_set` directly with a hand-built `LocalModelPatch` —
/// this is what an actual MCP caller sends). `local_model_get` reads every
/// rung's own command line and derived numbers back, and `clear: "ladder"`
/// round-trips to not-a-ladder.
#[tokio::test]
async fn ladder_set_and_get_round_trip_over_the_wire() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let dir = tempfile::tempdir().unwrap();
    // Same architecture, no tokenizer keys at all (as `ladder_models.rs`'s own
    // fixture): every rung's signature is trivially identical, so rule 5
    // never blocks this round trip.
    synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    synth::chat("qwen3", 65536).write_to(&dir.path().join("top.gguf"));

    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "laddered", "gguf_path": "base.gguf",
                "ctx_size": 32768, "parallel": 2, "n_predict": 4096,
                "ladder": "[{\"gguf_path\": \"top.gguf\", \"ctx_size\": 65536}]",
            }),
        )
        .await,
    );
    assert_eq!(created["ok"], true, "{created}");

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "laddered" }),
        )
        .await,
    );
    assert_eq!(
        got["ladder"],
        json!([{ "gguf_path": "top.gguf", "ctx_size": 65536 }]),
        "{got}"
    );
    let rungs = got["rungs"].as_array().unwrap_or_else(|| panic!("{got}"));
    assert_eq!(rungs.len(), 2, "{got}");
    assert_eq!(rungs[0]["rung"], 1);
    assert_eq!(rungs[0]["of"], 2);
    assert_eq!(rungs[0]["gguf_path"], "base.gguf");
    assert_eq!(rungs[0]["per_slot_ctx"], 16384);
    assert!(
        rungs[0]["command_line"]
            .as_str()
            .unwrap()
            .contains("base.gguf"),
        "{got}"
    );
    assert_eq!(rungs[1]["rung"], 2);
    assert_eq!(rungs[1]["gguf_path"], "top.gguf");
    assert_eq!(rungs[1]["per_slot_ctx"], 32768);
    assert_eq!(rungs[1]["switchover"], 32768 - 4096);
    assert!(
        rungs[1]["command_line"]
            .as_str()
            .unwrap()
            .contains("top.gguf"),
        "{got}"
    );

    let cleared = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({ "action": "update", "model_id": "laddered", "clear": "ladder" }),
        )
        .await,
    );
    assert_eq!(cleared["ok"], true, "{cleared}");
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "laddered" }),
        )
        .await,
    );
    assert_eq!(got["ladder"], json!([]), "{got}");
    assert_eq!(got["rungs"], Value::Null, "{got}");
}

/// `hoist_ladder_arg`'s leniency: a caller that ignores the declared string
/// schema and sends the array directly still works — the same "an object
/// where a string is declared" tolerance `capabilities_override` extends the
/// other way round.
#[tokio::test]
async fn ladder_set_also_accepts_a_literal_array_argument() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let dir = tempfile::tempdir().unwrap();
    synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    synth::chat("qwen3", 65536).write_to(&dir.path().join("top.gguf"));

    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "laddered-2", "gguf_path": "base.gguf",
                "ctx_size": 32768, "parallel": 2, "n_predict": 4096,
                "ladder": [{ "gguf_path": "top.gguf", "ctx_size": 65536 }],
            }),
        )
        .await,
    );
    assert_eq!(created["ok"], true, "{created}");
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "laddered-2" }),
        )
        .await,
    );
    assert_eq!(
        got["ladder"],
        json!([{ "gguf_path": "top.gguf", "ctx_size": 65536 }]),
        "{got}"
    );
}

/// Review third pass, T3: every other optional string in this module means
/// "leave unchanged" when empty (`ops::opt`) — an agent that fills every
/// optional field with "" is common with flat-string schemas, and `ladder`
/// must follow the same convention rather than silently clearing an
/// existing one. Clearing stays explicit (`clear: "ladder"`, or `"[]"`).
#[tokio::test]
async fn an_empty_string_ladder_argument_leaves_it_unchanged() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let dir = tempfile::tempdir().unwrap();
    synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    synth::chat("qwen3", 65536).write_to(&dir.path().join("top.gguf"));

    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "laddered-3", "gguf_path": "base.gguf",
                "ctx_size": 32768, "parallel": 2, "n_predict": 4096,
                "ladder": "[{\"gguf_path\": \"top.gguf\", \"ctx_size\": 65536}]",
            }),
        )
        .await,
    );

    let updated = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "update", "model_id": "laddered-3", "idle_seconds": 600,
                "ladder": "",
            }),
        )
        .await,
    );
    assert_eq!(updated["ok"], true, "{updated}");

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "laddered-3" }),
        )
        .await,
    );
    assert_eq!(
        got["ladder"],
        json!([{ "gguf_path": "top.gguf", "ctx_size": 65536 }]),
        "an empty ladder string must not clear it: {got}"
    );
    assert_eq!(
        got["idle_seconds"], 600,
        "the rest of the update still applied: {got}"
    );
}

/// Every tool that only reads must be reachable at `read_only`, and every tool
/// that changes something must not be. The gate is per-tool metadata, so a new
/// tool added with the wrong flag is exactly the mistake worth catching.
#[tokio::test]
async fn the_new_read_tools_are_available_without_write_access() {
    let base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let sid = initialize(&base).await;
    let names = list_tool_names(&base, &sid).await;

    for readable in [
        "lmgw__local_model_get",
        "lmgw__gguf_files",
        "lmgw__model_inspect",
        "lmgw__local_model_plan",
        "lmgw__llama_flags",
        "lmgw__hf_repo",
        "lmgw__hf_downloads",
        // Looking at the corpora and at the queue of what agents asked for
        // changes nothing, so a read-only grant keeps both.
        "lmgw__docs_corpora",
        "lmgw__docs_requests",
    ] {
        assert!(names.iter().any(|n| n == readable), "missing {readable}");
    }
    for gated in [
        "lmgw__hf_add",
        "lmgw__hf_set",
        // Loads a model off disk and pins VRAM — not a "read only" grant.
        "lmgw__local_model_test",
        // Creating, crawling or dropping a corpus is the owner's call.
        "lmgw__docs_corpus_set",
        "lmgw__docs_ingest",
        "lmgw__docs_request_set",
    ] {
        assert!(!names.iter().any(|n| n == gated), "{gated} leaked");
        let refused = call(&base, &sid, gated, json!({ "action": "delete" })).await;
        assert!(is_error(&refused), "{gated} should refuse at read_only");
    }
}

// ---------------------------------------------------------------------------
// GPU hold (gpu-hold design §6/§7.10)
// ---------------------------------------------------------------------------

/// `lmgw__hold_set` writes, so it follows every other mutation through the
/// mode gate: listed at `full`, absent at `read_only` (with an actionable
/// refusal if called anyway), and absent along with everything else at `off`.
#[tokio::test]
async fn hold_set_is_gated_like_every_other_mutation() {
    let full_base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let full_sid = initialize(&full_base).await;
    assert!(
        list_tool_names(&full_base, &full_sid)
            .await
            .iter()
            .any(|n| n == "lmgw__hold_set"),
        "lmgw__hold_set must be listed at full"
    );

    let off_base = serve_with(state_with_mode(SelfAdmin::Off).await).await;
    let off_sid = initialize(&off_base).await;
    assert!(
        list_tool_names(&off_base, &off_sid).await.is_empty(),
        "self_admin=off must expose no tools at all"
    );

    let ro_base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let ro_sid = initialize(&ro_base).await;
    assert!(
        !list_tool_names(&ro_base, &ro_sid)
            .await
            .iter()
            .any(|n| n == "lmgw__hold_set"),
        "lmgw__hold_set must not be listed at read_only — it mutates"
    );
    let refused = call(&ro_base, &ro_sid, "lmgw__hold_set", json!({"active": true})).await;
    assert!(is_error(&refused), "{}", text_of(&refused));
    assert!(
        text_of(&refused).contains("read_only"),
        "unhelpful refusal: {}",
        text_of(&refused)
    );
}

/// Calling `lmgw__hold_set` under `full` really flips the switch: the next
/// `lmgw__status` sees `vram.hold_active`, and `lmgw__settings` carries the
/// `hold` block with both `active` and the `fallback_alias` that
/// `lmgw__settings_set` just wrote.
///
/// Both halves are asserted through the *tools*, because that is the whole
/// contract an agent has here: the switch is settable only through
/// `lmgw__hold_set` (engaging it stops containers, a side effect the generic
/// settings call must not grow) while the fallback is settable only through
/// `lmgw__settings_set` — and neither is any use if the read tools do not show
/// what the other one did.
#[tokio::test]
async fn hold_set_flips_the_hold_and_every_read_tool_sees_it() {
    let state = state_with_mode(SelfAdmin::Full).await;
    // A real cloud alias: `hold_fallback_alias` is validated at set time and
    // refuses anything that does not resolve, so the fixture has to have one.
    let upstream = store::insert_upstream(
        &state.db,
        &store::NewUpstream {
            name: "cloud".into(),
            protocol: lmgw_core::config::Protocol::Openai,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
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
        &store::NewAlias {
            alias: "cloud-chat".into(),
            upstream_id: upstream,
            upstream_model_id: "gpt-cloud".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let before = payload(&call(&base, &sid, "lmgw__status", json!({})).await);
    assert_eq!(before["vram"]["hold_active"], false, "{before}");

    let set = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({"hold_fallback_alias": "cloud-chat"}),
    )
    .await;
    assert!(!is_error(&set), "{}", text_of(&set));

    let out = payload(&call(&base, &sid, "lmgw__hold_set", json!({"active": true})).await);
    assert_eq!(out["active"], true, "{out}");
    assert_eq!(
        out["fallback_alias"], "cloud-chat",
        "the op answers with what a held chat model will actually be served by: {out}"
    );

    let after = payload(&call(&base, &sid, "lmgw__status", json!({})).await);
    assert_eq!(after["vram"]["hold_active"], true, "{after}");

    let settings = payload(&call(&base, &sid, "lmgw__settings", json!({})).await);
    assert_eq!(settings["hold"]["active"], true, "{settings}");
    assert_eq!(
        settings["hold"]["fallback_alias"], "cloud-chat",
        "the two setters write one block, and this read tool is where an agent sees it: \
         {settings}"
    );

    // And back off again — the switch is a switch, not a latch.
    let out = payload(&call(&base, &sid, "lmgw__hold_set", json!({"active": false})).await);
    assert_eq!(out["active"], false, "{out}");
    let after = payload(&call(&base, &sid, "lmgw__status", json!({})).await);
    assert_eq!(after["vram"]["hold_active"], false, "{after}");
}

/// `vram.fallback_on_external` (candidate-aliases design §4.7, §12.25) reads
/// as on by default next to the hold block, and `lmgw__settings_set` writes
/// it back — the same round trip as the hold's fallback alias above, for the
/// switch that decides when *that* fallback (or a per-model one) answers
/// instead of queueing.
#[tokio::test]
async fn settings_set_fallback_on_external_round_trips() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let before = payload(&call(&base, &sid, "lmgw__settings", json!({})).await);
    assert_eq!(before["vram"]["fallback_on_external"], true, "{before}");

    let set = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({"fallback_on_external": false}),
    )
    .await;
    assert!(!is_error(&set), "{}", text_of(&set));

    let after = payload(&call(&base, &sid, "lmgw__settings", json!({})).await);
    assert_eq!(after["vram"]["fallback_on_external"], false, "{after}");
}

/// `realtime` is one JSON-encoded string on the flat tool schema (realtime
/// design §12), hoisted into the same patch the dashboard saves — with the
/// same refusals, and `""` leaving the section alone.
#[tokio::test]
async fn settings_set_realtime_takes_a_json_string() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;

    let set = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({"realtime": r#"{"warm_on_connect": false, "semantic_vad": {"high": {"max_wait_ms": 2500}}}"#}),
    )
    .await;
    let out = payload(&set);
    assert_eq!(
        out["changed"],
        json!(["realtime.semantic_vad", "realtime.warm_on_connect"]),
        "{out}"
    );
    let after = payload(&call(&base, &sid, "lmgw__settings", json!({})).await);
    assert_eq!(after["realtime"]["warm_on_connect"], false, "{after}");
    assert_eq!(
        after["realtime"]["semantic_vad"]["high"]["max_wait_ms"],
        2500
    );
    assert_eq!(after["realtime"]["semantic_vad"]["high"]["floor"], 0.1);

    let refused = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({"realtime": r#"{"max_message_mb": 0, "max_frame_mb": 0}"#}),
    )
    .await;
    assert!(is_error(&refused));
    assert!(
        text_of(&refused).contains("realtime.max_frame_mb"),
        "{}",
        text_of(&refused)
    );
    let bad_json = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({"realtime": "{nope"}),
    )
    .await;
    assert!(is_error(&bad_json));
    assert!(text_of(&bad_json).contains("realtime: invalid JSON"));
    let empty = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({"realtime": "", "retention_days": 7}),
    )
    .await;
    assert_eq!(payload(&empty)["changed"], json!(["retention_days"]));
}

/// An unknown name inside the reserved namespace is a genuine protocol error,
/// not a tool error — the distinction the dispatcher draws in §14.
#[tokio::test]
async fn unknown_builtin_is_a_json_rpc_method_not_found() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;
    let (status, _, body) = post(
        &base,
        Some(&sid),
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "lmgw__does_not_exist", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["error"]["code"], -32601, "{body}");
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_reports_the_gateway() {
    let base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let sid = initialize(&base).await;
    let p = payload(&call(&base, &sid, "lmgw__status", json!({})).await);

    assert!(p["version"].is_string());
    assert!(p["uptime_seconds"].is_number());
    assert_eq!(p["self_admin"], "read_only");
    // No models are configured in this test's snapshot, so the per-model
    // runtime list (per-model-containers §3.2/§8, replacing the old fixed
    // three-container list) is empty rather than absent.
    assert_eq!(p["runtime"].as_array().unwrap().len(), 0);
    assert!(p["requests"]["total"].is_number());
    assert!(p["counts"]["upstreams"].is_number());
}

#[tokio::test]
async fn upstreams_never_returns_an_api_key() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({
            "action": "create", "name": "secret-up", "protocol": "openai",
            "base_url": "https://api.example.com/v1", "api_key": "sk-super-secret",
        }),
    )
    .await;

    let listed = call(&base, &sid, "lmgw__upstreams", json!({})).await;
    let raw = text_of(&listed);
    assert!(
        !raw.contains("sk-super-secret"),
        "the API key leaked into the tool output: {raw}"
    );
    let p: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(p["upstreams"][0]["api_key"], "<set>");
}

// ---------------------------------------------------------------------------
// Mutations reach the database *and* the live snapshot
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_upstream_and_alias_makes_a_routable_model() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({
                "action": "create", "name": "acme", "protocol": "openai",
                "base_url": "https://api.acme.test/v1",
            }),
        )
        .await,
    );
    assert_eq!(created["ok"], true);

    // The alias references the upstream by *name* — what a caller reading
    // lmgw__upstreams already has, without needing the numeric id.
    payload(
        &call(
            &base,
            &sid,
            "lmgw__model_set",
            json!({
                "action": "create", "alias": "fast",
                "upstream": "acme", "upstream_model": "acme-turbo",
            }),
        )
        .await,
    );

    // Visible to the tool plane...
    let models = payload(&call(&base, &sid, "lmgw__models", json!({ "kind": "alias" })).await);
    let names: Vec<&str> = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["fast"]);

    // ...and, crucially, to the live snapshot the request hot path reads, which
    // only holds if each mutation reloaded it.
    let route = inspect
        .snapshot()
        .resolve("fast")
        .expect("alias must route");
    assert_eq!(route.upstream.name, "acme");
    assert_eq!(route.upstream_model, "acme-turbo");
}

/// `lmgw__models` joins the same builder `GET /v1/models` renders from
/// (model-capabilities design §8 item 9), by name, rather than re-deriving:
/// a local chat row gets `capabilities.task == "chat"` and its configured
/// `n_predict` back as `max_output_tokens`, an aux row gets
/// `capabilities.task == "embedding"`.
#[tokio::test]
async fn models_carries_the_same_capabilities_v1_models_publishes() {
    let state = state_with_mode(SelfAdmin::Full).await;

    // A real (synthetic) GGUF on disk in each class's own models dir — the
    // capability builder returns `capabilities: None` for a row whose
    // weights it could not read, so a fake path would not exercise the field
    // this test is about.
    let chat_dir = tempfile::tempdir().unwrap();
    let aux_dir = tempfile::tempdir().unwrap();
    synth::chat("qwen35", 32768).write_to(&chat_dir.path().join("chat.gguf"));
    synth::embedding("qwen3", 3, 32768).write_to(&aux_dir.path().join("embed.gguf"));

    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = chat_dir.path().display().to_string();
    settings.aux_router.models_dir = aux_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "chat-cap", "gguf_path": "chat.gguf",
                "n_predict": 512,
            }),
        )
        .await,
    );
    payload(
        &call(
            &base,
            &sid,
            "lmgw__aux_model_set",
            json!({
                "action": "create", "model_id": "emb-cap", "gguf_path": "embed.gguf",
                "kind": "embed",
            }),
        )
        .await,
    );

    let models = payload(&call(&base, &sid, "lmgw__models", json!({})).await);
    let list = models["models"].as_array().unwrap();

    let chat = list
        .iter()
        .find(|m| m["name"] == "chat-cap")
        .expect("chat row listed");
    assert_eq!(chat["capabilities"]["task"], "chat", "{chat}");
    assert_eq!(chat["max_output_tokens"], 512, "{chat}");

    let aux = list
        .iter()
        .find(|m| m["name"] == "embed/emb-cap")
        .expect("aux row listed");
    assert_eq!(aux["capabilities"]["task"], "embedding", "{aux}");
}

#[tokio::test]
async fn update_is_partial_and_preserves_the_stored_key() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({
                "action": "create", "name": "acme", "protocol": "openai",
                "base_url": "https://api.acme.test/v1", "api_key": "sk-keep-me",
                "timeout_ms": 45000,
            }),
        )
        .await,
    );
    let id = created["id"].as_i64().unwrap();

    // Change one field only. Everything unmentioned — including the secret that
    // reads back as `<set>` — must survive untouched.
    payload(
        &call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({ "action": "update", "id": id, "timeout_ms": 30000 }),
        )
        .await,
    );
    let stored = store::get_upstream(&inspect.db, id).await.unwrap().unwrap();
    assert_eq!(stored.base_url, "https://api.acme.test/v1");
    assert_eq!(stored.name, "acme");
    assert_eq!(stored.timeout_ms, 30000);
    assert_eq!(stored.api_key.as_deref(), Some("sk-keep-me"));

    // Moving it to another host without the key for that host is refused
    // (review G-3): the stored key is never sent where it was not given.
    let refused = call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "update", "id": id, "base_url": "https://new.acme.test/v1" }),
    )
    .await;
    assert!(is_error(&refused), "{refused}");
    assert!(
        text_of(&refused).contains("needs the key for that address in the same call"),
        "{}",
        text_of(&refused)
    );
    let stored = store::get_upstream(&inspect.db, id).await.unwrap().unwrap();
    assert_eq!(
        stored.base_url, "https://api.acme.test/v1",
        "nothing changed"
    );
    // Every action that applies base_url is checked, not only update (the
    // review's verification V-1): enable and disable move the row too.
    for action in ["enable", "disable"] {
        let refused = call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({ "action": action, "id": id, "base_url": "https://new.acme.test/v1" }),
        )
        .await;
        assert!(is_error(&refused), "{action}: {refused}");
        assert!(
            text_of(&refused).contains("needs the key for that address in the same call"),
            "{action}: {}",
            text_of(&refused)
        );
        let stored = store::get_upstream(&inspect.db, id).await.unwrap().unwrap();
        assert_eq!(stored.base_url, "https://api.acme.test/v1", "{action}");
        assert!(
            stored.enabled,
            "{action}: nothing changed, enabled included"
        );
        assert_eq!(stored.api_key.as_deref(), Some("sk-keep-me"), "{action}");
    }
    // The same address written differently is no move.
    payload(
        &call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({ "action": "update", "id": id, "base_url": "https://API.acme.test/v1/" }),
        )
        .await,
    );
    // With the key for the new host, it moves.
    payload(
        &call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({ "action": "update", "id": id, "base_url": "https://new.acme.test/v1",
                    "api_key": "sk-new-host" }),
        )
        .await,
    );
    let stored = store::get_upstream(&inspect.db, id).await.unwrap().unwrap();
    assert_eq!(stored.base_url, "https://new.acme.test/v1");
    assert_eq!(stored.api_key.as_deref(), Some("sk-new-host"));
}

/// An MCP server's stored headers follow its `url` the same way (review
/// G-3): a move without them is refused, whichever action carries it and
/// with `null` as no restatement, and a move that restates them — `""`
/// included, which sends none — goes through.
#[tokio::test]
async fn moving_an_mcp_server_needs_its_headers_restated() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;
    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__mcp_server_set",
            json!({
                "action": "create", "name": "tickets", "transport": "http",
                "url": "https://mcp.tickets.test/mcp",
                "headers": "Authorization: Bearer secret-token",
                "autostart": false, "enabled": false,
            }),
        )
        .await,
    );
    let id = created["id"].as_i64().unwrap();
    let refused = call(
        &base,
        &sid,
        "lmgw__mcp_server_set",
        json!({ "action": "update", "id": id, "url": "https://x.example/mcp" }),
    )
    .await;
    assert!(is_error(&refused), "{refused}");
    assert!(
        text_of(&refused).contains("needs the headers for that address"),
        "{}",
        text_of(&refused)
    );
    let row = store::get_mcp_server(&inspect.db, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.url.as_deref(), Some("https://mcp.tickets.test/mcp"));
    // `null` keeps the stored headers, so it restates nothing (V-2), and
    // enable and disable apply url as update does (V-1).
    for args in [
        json!({ "action": "update", "id": id, "url": "https://x.example/mcp", "headers": null }),
        json!({ "action": "enable", "id": id, "url": "https://x.example/mcp" }),
        json!({ "action": "disable", "id": id, "url": "https://x.example/mcp" }),
    ] {
        let refused = call(&base, &sid, "lmgw__mcp_server_set", args.clone()).await;
        assert!(is_error(&refused), "{args}: {refused}");
        assert!(
            text_of(&refused).contains("needs the headers for that address"),
            "{args}: {}",
            text_of(&refused)
        );
        let row = store::get_mcp_server(&inspect.db, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.url.as_deref(),
            Some("https://mcp.tickets.test/mcp"),
            "{args}"
        );
        assert!(!row.enabled, "{args}: nothing changed, enabled included");
        assert_eq!(row.headers.len(), 1, "{args}");
    }
    payload(
        &call(
            &base,
            &sid,
            "lmgw__mcp_server_set",
            json!({ "action": "update", "id": id, "url": "https://x.example/mcp", "headers": "" }),
        )
        .await,
    );
    let row = store::get_mcp_server(&inspect.db, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.url.as_deref(), Some("https://x.example/mcp"));
    assert!(row.headers.is_empty(), "the old credential did not travel");
}

/// The access settings are refused to every tool caller, the owner's own
/// plane included (review G-1): they change on Settings.
#[tokio::test]
async fn the_access_settings_are_no_tool_s_to_change() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;
    for args in [
        json!({ "auth_enabled": true }),
        json!({ "self_admin": "off" }),
        json!({ "bind_addr": "0.0.0.0:8787" }),
        json!({ "agent_origin_suffix": "lmgw.lan" }),
    ] {
        let key = args.as_object().unwrap().keys().next().unwrap().clone();
        let refused = call(&base, &sid, "lmgw__settings_set", args).await;
        assert!(is_error(&refused), "{key}: {refused}");
        assert!(
            text_of(&refused).starts_with(&format!(
                "{key} cannot be changed through lmgw's admin tools"
            )),
            "{}",
            text_of(&refused)
        );
    }
    let s = inspect.snapshot().settings.clone();
    assert!(!s.auth_enabled);
    assert_eq!(s.self_admin, SelfAdmin::Full);
}

#[tokio::test]
async fn enable_disable_round_trips() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let id = payload(
        &call(
            &base,
            &sid,
            "lmgw__upstream_set",
            json!({ "action": "create", "name": "acme", "protocol": "openai",
                    "base_url": "https://api.acme.test/v1" }),
        )
        .await,
    )["id"]
        .as_i64()
        .unwrap();

    call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "disable", "id": id }),
    )
    .await;
    assert!(
        !store::get_upstream(&inspect.db, id)
            .await
            .unwrap()
            .unwrap()
            .enabled
    );

    call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "enable", "id": id }),
    )
    .await;
    assert!(
        store::get_upstream(&inspect.db, id)
            .await
            .unwrap()
            .unwrap()
            .enabled
    );
}

#[tokio::test]
async fn settings_set_cannot_widen_its_own_gate() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    // `self_admin` is not a declared property, and the schema is closed — the
    // attempt is rejected outright rather than quietly ignored.
    let result = call(
        &base,
        &sid,
        "lmgw__settings_set",
        json!({ "self_admin": "full", "retention_days": 7 }),
    )
    .await;
    assert!(is_error(&result), "unknown argument must be rejected");

    // A legitimate change still works, and the gate is untouched.
    payload(
        &call(
            &base,
            &sid,
            "lmgw__settings_set",
            json!({ "retention_days": 7 }),
        )
        .await,
    );
    let s = inspect.snapshot();
    assert_eq!(s.settings.retention_days, 7);
    assert_eq!(s.settings.self_admin, SelfAdmin::Full);
}

#[tokio::test]
async fn a_bad_argument_is_a_readable_tool_error() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;

    // Missing required fields are reported together, so a caller needs one
    // correction rather than one round trip per field.
    let result = call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "create", "name": "incomplete" }),
    )
    .await;
    assert!(is_error(&result));
    let msg = text_of(&result);
    assert!(msg.contains("base_url"), "{msg}");
    assert!(msg.contains("protocol"), "{msg}");

    // An action that doesn't exist lists the ones that do.
    let result = call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "obliterate", "id": 1 }),
    )
    .await;
    assert!(is_error(&result));
    assert!(text_of(&result).contains("create"), "{}", text_of(&result));
}

// ---------------------------------------------------------------------------
// The two guards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registering_the_gateways_own_mcp_endpoint_is_refused() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let bind = state.snapshot().settings.bind_addr.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    let result = call(
        &base,
        &sid,
        "lmgw__mcp_server_set",
        json!({
            "action": "create", "name": "myself", "transport": "http",
            "url": format!("http://{bind}/mcp"),
        }),
    )
    .await;
    assert!(is_error(&result), "self-registration must be refused");
    let msg = text_of(&result);
    assert!(msg.contains("own MCP endpoint"), "{msg}");

    // A different server on the same host is still fine.
    let ok = call(
        &base,
        &sid,
        "lmgw__mcp_server_set",
        json!({
            "action": "create", "name": "elsewhere", "transport": "http",
            "url": "https://mcp.example.com/mcp",
        }),
    )
    .await;
    assert!(!is_error(&ok), "{}", text_of(&ok));
}

#[tokio::test]
async fn a_southbound_server_cannot_claim_the_reserved_prefix() {
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;

    let result = call(
        &base,
        &sid,
        "lmgw__mcp_server_set",
        json!({
            "action": "create", "name": "impostor", "transport": "http",
            "url": "https://mcp.example.com/mcp", "tool_prefix": "lmgw",
        }),
    )
    .await;
    assert!(is_error(&result));
    assert!(
        text_of(&result).contains("reserved"),
        "{}",
        text_of(&result)
    );
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn self_admin_calls_are_written_to_the_request_log() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let inspect = state.clone();
    let base = serve_with(state).await;
    let sid = initialize(&base).await;

    call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "create", "name": "audited", "protocol": "openai",
                "base_url": "https://api.acme.test/v1" }),
    )
    .await;
    // A refused call must be auditable too — that's the one you most want to see.
    call(
        &base,
        &sid,
        "lmgw__upstream_set",
        json!({ "action": "delete", "id": 99999 }),
    )
    .await;

    let rows = store::query_logs(
        &inspect.db,
        &store::LogFilter {
            alias: None,
            upstream_name: None,
            errors_only: false,
            limit: 50,
            before_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let calls: Vec<_> = rows
        .iter()
        .filter(|r| r.mcp_tool.as_deref() == Some("lmgw__upstream_set"))
        .collect();
    assert_eq!(calls.len(), 2, "both calls should be logged");
    assert!(calls.iter().all(|r| r.ingress_proto == "mcp"));
    assert!(calls
        .iter()
        .all(|r| r.upstream_name.as_deref() == Some("lmgw (self-admin)")));

    // The failure is recorded as a failure, with its reason.
    let failed = calls
        .iter()
        .find(|r| r.status != 200)
        .expect("a failed row");
    assert_eq!(failed.error_kind.as_deref(), Some("tool_error"));
    assert!(failed.error_msg.as_deref().unwrap().contains("99999"));
}

// ---------------------------------------------------------------------------
// Local models: full parameter coverage and read-back
// ---------------------------------------------------------------------------

/// The gap that motivated the expanded surface: a multimodal, speculatively
/// decoded model could not be expressed through the tool plane at all — the
/// projector and drafter had no fields, so they had to be typed into the
/// freeform args escape hatch with in-container paths.
///
/// Asserts the whole round trip: create with every interesting field, read it
/// back (which was itself impossible before — the list view returns only
/// names), and confirm the rendered preset carries the right keys with the
/// `/models/` prefix applied exactly once.
#[tokio::test]
async fn a_multimodal_speculative_model_round_trips_through_the_tool_plane() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let created = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create",
            "model_id": "muse-glimmer-30b",
            "gguf_path": "unsloth/Muse-Glimmer-30B-GGUF/Muse-Glimmer-30B-UD-Q4_K_XL.gguf",
            "mmproj_path": "unsloth/Muse-Glimmer-30B-GGUF/mmproj-kquant.gguf",
            "draft_gguf_path": "unsloth/Muse-Glimmer-30B-GGUF/dflash-kquant.gguf",
            "spec_type": "draft-dflash",
            "spec_draft_n_max": 16,
            "ctx_size": 131072,
            "n_gpu_layers": 999,
            "flash_attn": "on",
            "cache_type_k": "q8_0",
            "cache_type_v": "q8_0",
            "jinja": true,
            "temp": 1.0,
            "top_p": 0.95,
            "top_k": 64,
            "reasoning_format": "auto"
        }),
    )
    .await;
    assert!(!is_error(&created), "create failed: {}", text_of(&created));

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "muse-glimmer-30b" }),
        )
        .await,
    );

    // Paths stay relative on the way out — the caller never deals with the
    // container's mount point.
    assert_eq!(
        got["params"]["mmproj_path"],
        "unsloth/Muse-Glimmer-30B-GGUF/mmproj-kquant.gguf"
    );
    assert_eq!(got["params"]["spec_type"], "draft-dflash");
    assert_eq!(got["params"]["spec_draft_n_max"], 16);
    assert_eq!(got["params"]["ctx_size"], 131072);
    assert_eq!(got["params"]["top_p"], 0.95);
    assert!(got["params"]["jinja"].as_bool().unwrap());

    // … and the command line it renders is what its container is started
    // with (§3.6 — the preset section this used to assert died with router
    // mode).
    let cmd = got["command_line"].as_str().unwrap();
    assert!(
        cmd.contains("--mmproj /models/unsloth/Muse-Glimmer-30B-GGUF/mmproj-kquant.gguf"),
        "projector missing or mis-prefixed:\n{cmd}"
    );
    assert!(
        cmd.contains("--model-draft /models/unsloth/Muse-Glimmer-30B-GGUF/dflash-kquant.gguf"),
        "drafter missing or mis-prefixed:\n{cmd}"
    );
    assert!(cmd.contains("--spec-type draft-dflash"), "{cmd}");
    assert!(cmd.contains("--ctx-size 131072"), "{cmd}");
    assert!(
        cmd.contains("--alias muse-glimmer-30b"),
        "direct mode must name the model:\n{cmd}"
    );
    assert!(!cmd.contains("/models//"), "double slash in a path:\n{cmd}");
}

/// `None` means "leave as is", so unsetting needs its own verb. Without it a
/// caller could add a drafter but never remove one.
#[tokio::test]
async fn clear_unsets_fields_that_omission_would_preserve() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create", "model_id": "m", "gguf_path": "m.gguf",
            "spec_type": "draft-mtp", "spec_draft_n_max": 4, "ctx_size": 8192
        }),
    )
    .await;

    // An unrelated update must not disturb the drafter settings …
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "m", "ctx_size": 4096 }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "m" }),
        )
        .await,
    );
    assert_eq!(got["params"]["spec_type"], "draft-mtp");
    assert_eq!(got["params"]["ctx_size"], 4096);

    // … but naming them in `clear` must.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "m", "clear": "spec_type,spec_draft_n_max" }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "m" }),
        )
        .await,
    );
    assert!(got["params"]["spec_type"].is_null(), "{got}");
    assert!(got["params"]["spec_draft_n_max"].is_null(), "{got}");
    assert_eq!(
        got["params"]["ctx_size"], 4096,
        "clear must not touch others"
    );

    let bad = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "m", "clear": "nonsense_field" }),
    )
    .await;
    assert!(is_error(&bad), "an unknown clear target must be reported");
    assert!(text_of(&bad).contains("nonsense_field"));
}

/// A projector llama.cpp decodes non-causally, under its default ubatch
/// (512), loads fine and then aborts llama-server on the first full-page image
/// (Gemma 4, 2026-09-24). Every read of the row has to say so, naming the
/// assert and the fix, as an advisory: the model starts, so none of them may
/// refuse the row or call it broken. A causal projector with the same batch
/// sizes is healthy and must stay quiet.
#[tokio::test]
async fn a_projector_row_below_the_ubatch_it_needs_gets_an_advisory_everywhere() {
    let state = state_with_mode(SelfAdmin::Full).await;

    // Headers that read as Gemma 4 12B's projector and a Qwen3.5 one: the
    // type is what decides, so a synthetic file is as good as a real one.
    let dir = tempfile::tempdir().unwrap();
    synth::chat("gemma4", 131072).write_to(&dir.path().join("gemma4/w.gguf"));
    let mut gemma = synth::Header::default();
    gemma
        .str("general.architecture", "clip")
        .str("general.type", "mmproj")
        .str("clip.vision.projector_type", "gemma4uv")
        .str("clip.audio.projector_type", "gemma4ua");
    gemma.write_to(&dir.path().join("gemma4/mmproj.gguf"));
    synth::chat("qwen35", 262144).write_to(&dir.path().join("qwen/w.gguf"));
    let mut qwen = synth::Header::default();
    qwen.str("general.architecture", "clip")
        .str("general.type", "mmproj")
        .str("clip.projector_type", "qwen3vl_merger")
        .u32("clip.vision.block_count", 27);
    qwen.write_to(&dir.path().join("qwen/mmproj.gguf"));
    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;
    let names_the_assert = |v: &Value| {
        v.as_array().unwrap().iter().any(|p| {
            let p = p.as_str().unwrap_or("");
            p.contains("non-causal attention requires n_ubatch >= n_tokens")
                && p.contains("Set ubatch_size to at least 1280")
        })
    };
    let check = |id: &'static str| {
        let (base, sid) = (base.clone(), sid.clone());
        async move {
            payload(
                &call(
                    &base,
                    &sid,
                    "lmgw__local_model_check",
                    json!({ "model_id": id }),
                )
                .await,
            )
        }
    };

    // 1. The set path: saved, with the advisory beside the warnings, not in
    //    them.
    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "gemma4", "gguf_path": "gemma4/w.gguf",
                "mmproj_path": "gemma4/mmproj.gguf",
            }),
        )
        .await,
    );
    assert_eq!(created["ok"], true, "an advisory must not block the save");
    assert!(names_the_assert(&created["advisories"]), "{created}");
    assert!(!names_the_assert(&created["warnings"]), "{created}");

    // 2. The read: an advisory, stating the type, and no problem.
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "gemma4" }),
        )
        .await,
    );
    assert!(names_the_assert(&got["advisories"]), "{got}");
    assert!(!names_the_assert(&got["problems"]), "{got}");
    let advisory = got["advisories"][0].as_str().unwrap();
    assert!(
        advisory.contains("(gemma4uv projector) with non-causal attention"),
        "a known type is stated, not hedged: {advisory}"
    );

    // 3. The check: reported, and the row is still ok — the model starts.
    let checked = check("gemma4").await;
    let row = &checked["models"][0];
    assert!(names_the_assert(&row["advisories"]), "{checked}");
    assert_eq!(
        row["ok"], true,
        "an advisory alone must not mark a row broken"
    );
    assert_eq!(row["problems"].as_array().unwrap().len(), 0, "{checked}");
    assert_eq!(checked["broken"], 0, "{checked}");
    assert_eq!(checked["with_advisories"], 1, "{checked}");

    // 4. At the floor the advisory is gone.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "gemma4", "ubatch_size": 1280 }),
    )
    .await;
    let checked = check("gemma4").await;
    assert_eq!(
        checked["models"][0]["advisories"].as_array().unwrap().len(),
        0,
        "{checked}"
    );

    // 5. The live Qwen3.5 shape: a causal projector, batch_size 1024 and the
    //    default ubatch. Healthy, so nothing to report.
    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "qwen", "gguf_path": "qwen/w.gguf",
                "mmproj_path": "qwen/mmproj.gguf", "batch_size": 1024,
            }),
        )
        .await,
    );
    assert_eq!(created["advisories"], json!([]), "{created}");
    let checked = check("qwen").await;
    assert_eq!(checked["models"][0]["ok"], true, "{checked}");
    assert_eq!(checked["models"][0]["advisories"], json!([]), "{checked}");

    // 6. A projector whose header cannot be read still gets the advisory,
    //    worded as what it is: a question lmgw could not answer.
    std::fs::create_dir_all(dir.path().join("broken")).unwrap();
    std::fs::write(dir.path().join("broken/mmproj.gguf"), b"not a gguf").unwrap();
    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "mystery", "gguf_path": "qwen/w.gguf",
                "mmproj_path": "broken/mmproj.gguf",
            }),
        )
        .await,
    );
    assert!(names_the_assert(&created["advisories"]), "{created}");
    assert!(
        created["advisories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("could not tell whether")),
        "{created}"
    );

    // 7. A projector file that has gone missing after the save is the
    //    problem; there is no batch advice to give about a file that cannot
    //    load.
    std::fs::remove_file(dir.path().join("broken/mmproj.gguf")).unwrap();
    let checked = check("mystery").await;
    let row = &checked["models"][0];
    assert_eq!(row["ok"], false, "{checked}");
    assert!(
        row["problems"][0]
            .as_str()
            .unwrap()
            .contains("mmproj_path 'broken/mmproj.gguf' is missing"),
        "{checked}"
    );
    assert_eq!(row["advisories"], json!([]), "{checked}");

    // 8. Group apply reports the advisory apart from the problems, too.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "gemma4", "clear": "ubatch_size" }),
    )
    .await;
    let applied = call(
        &base,
        &sid,
        "lmgw__container",
        json!({ "target": "chat", "action": "apply" }),
    )
    .await;
    let applied = payload(&applied);
    assert_eq!(applied["models_with_advisories"], 1, "{applied}");
    assert_eq!(applied["advisories"][0]["model_id"], "gemma4", "{applied}");
    assert!(
        applied["problems"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["model_id"] != "gemma4"),
        "{applied}"
    );
}

/// A pool-guarded row (`LlamaParams::pool_guarded`: unified KV explicitly on,
/// more than one slot, `n_predict` set — unified-KV design §12) reserves
/// against a per-image token bound before the gate admits an image request,
/// via `gate::count::image_token_bound`. A causal projector with no measured
/// ceiling and no `--image-max-tokens` gives that function nothing to bound,
/// so the row should carry an advisory saying image requests to it are
/// refused until the flag is set — the model still loads and serves text
/// fine, so this must never become a problem. The identical row *without*
/// the guard (kv_unified/n_predict both unset) stays quiet: an auto-slot row
/// is deliberately left unguarded, so there is nothing for the gate to
/// refuse.
#[tokio::test]
async fn a_pool_guarded_row_with_no_image_bound_gets_an_advisory() {
    let state = state_with_mode(SelfAdmin::Full).await;

    // A causal projector (qwen3vl_merger, as in the ubatch test above):
    // healthy under any batch size, but `image_token_bound` still refuses it
    // absent a configured `--image-max-tokens`, because a causal projector's
    // image cost is not fixed.
    let dir = tempfile::tempdir().unwrap();
    synth::chat("qwen35", 262144).write_to(&dir.path().join("qwen/w.gguf"));
    let mut qwen = synth::Header::default();
    qwen.str("general.architecture", "clip")
        .str("general.type", "mmproj")
        .str("clip.projector_type", "qwen3vl_merger")
        .u32("clip.vision.block_count", 27);
    qwen.write_to(&dir.path().join("qwen/mmproj.gguf"));
    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;
    let names_the_gate_advisory = |v: &Value| {
        v.as_array().unwrap().iter().any(|p| {
            let p = p.as_str().unwrap_or("");
            p.contains("pool-guarded") && p.contains("--image-max-tokens is set in the row's args")
        })
    };

    // 1. Guarded: kv_unified on, n_predict set — the advisory fires.
    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "guarded", "gguf_path": "qwen/w.gguf",
                "mmproj_path": "qwen/mmproj.gguf", "kv_unified": true, "n_predict": 512,
                // The pool ledger needs a known size for an explicitly shared
                // pool (review finding 2) — this row has no `parallel`, so
                // it defaults to 4 auto slots (> 1) and needs one.
                "kv_unified_per_slot": 4096,
            }),
        )
        .await,
    );
    assert_eq!(created["ok"], true, "an advisory must not block the save");
    assert!(names_the_gate_advisory(&created["advisories"]), "{created}");

    // 2. The unguarded twin: same projector, same missing bound, no guard —
    //    nothing to report.
    let unguarded = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "unguarded", "gguf_path": "qwen/w.gguf",
                "mmproj_path": "qwen/mmproj.gguf",
            }),
        )
        .await,
    );
    assert_eq!(unguarded["advisories"], json!([]), "{unguarded}");

    // 3. Giving the guarded row a bound clears the advisory, same shape as
    //    the ubatch advisory's own floor case.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "guarded",
            "extra_args": "--image-max-tokens 1280",
        }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "guarded" }),
        )
        .await,
    );
    assert!(!names_the_gate_advisory(&got["advisories"]), "{got}");
}

/// The thinking controls that reach the chat template: a default effort level
/// (`--reasoning-effort`) and freeform template variables
/// (`--chat-template-kwargs`) — two flags feeding one variable namespace, so
/// the round trip has to prove both arrive and that an effort level written
/// into the kwargs does not end up configured twice.
#[tokio::test]
async fn reasoning_effort_and_custom_kwargs_both_reach_the_command_line() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let created = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create",
            "model_id": "qwen3",
            "gguf_path": "qwen3.gguf",
            "jinja": true,
            "reasoning_preserve": true,
            "reasoning_effort": "medium",
            "chat_template_kwargs": "{\"preserve_thinking\": true}",
            "n_predict": 4096
        }),
    )
    .await;
    assert!(!is_error(&created), "create failed: {}", text_of(&created));

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3" }),
        )
        .await,
    );
    assert_eq!(got["params"]["reasoning_effort"], "medium");
    assert_eq!(got["params"]["reasoning_preserve"], true);
    assert_eq!(
        got["params"]["chat_template_kwargs"]["preserve_thinking"],
        true
    );
    assert_eq!(got["params"]["n_predict"], 4096);

    let cmd = got["command_line"].as_str().unwrap();
    // A bare switch, not `--reasoning-preserve true`: CLI semantics, not INI
    // ones (§3.6).
    assert!(cmd.contains("--reasoning-preserve "), "{cmd}");
    assert!(cmd.contains("--reasoning-effort medium"), "{cmd}");
    assert!(cmd.contains("--n-predict 4096"), "{cmd}");
    assert!(
        cmd.contains(r#"preserve_thinking"#),
        "kwargs missing:\n{cmd}"
    );

    // Malformed JSON fails on the way in, not minutes later at model load.
    let bad = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "qwen3", "chat_template_kwargs": "[1,2]" }),
    )
    .await;
    assert!(is_error(&bad), "a JSON array was accepted");
    assert!(text_of(&bad).contains("object"), "{}", text_of(&bad));

    // Each control clears on its own; "off" is a value, not an absence, so
    // clearing the switch has to leave the template default in charge.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "qwen3", "reasoning_preserve": false,
            "clear": "reasoning_effort, n_predict"
        }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3" }),
        )
        .await,
    );
    assert!(got["params"]["reasoning_effort"].is_null(), "{got}");
    assert_eq!(got["params"]["reasoning_preserve"], false);
    assert!(got["params"]["n_predict"].is_null(), "{got}");
    let cmd = got["command_line"].as_str().unwrap();
    // `false` on a default-on option renders the `--no-*` twin (§3.6).
    assert!(cmd.contains("--no-reasoning-preserve"), "{cmd}");
    assert!(!cmd.contains("--reasoning-effort"), "{cmd}");
    assert!(!cmd.contains("--n-predict"), "{cmd}");
    assert!(cmd.contains("preserve_thinking"), "{cmd}");

    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "qwen3",
            "clear": "reasoning_preserve,chat_template_kwargs"
        }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3" }),
        )
        .await,
    );
    let cmd = got["command_line"].as_str().unwrap();
    assert!(!cmd.contains("reasoning-preserve"), "{cmd}");
    assert!(!cmd.contains("--chat-template-kwargs"), "{cmd}");

    // An effort level typed into the kwargs folds into its own field rather
    // than being configured twice, under two flags, for one template variable.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "qwen3",
            "chat_template_kwargs": "{\"reasoning_effort\":\"none\",\"other\":1}"
        }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3" }),
        )
        .await,
    );
    assert_eq!(got["params"]["reasoning_effort"], "none");
    assert!(
        got["params"]["chat_template_kwargs"]["reasoning_effort"].is_null(),
        "{got}"
    );
    let cmd = got["command_line"].as_str().unwrap();
    assert!(cmd.contains("--reasoning-effort none"), "{cmd}");
    assert!(cmd.contains(r#"{"other":1}"#), "{cmd}");
}

/// The unified-KV toggle (unified-KV design §3.1): both fields round-trip
/// through `lmgw__local_model_get`, reach the command line as the flags the
/// design names, and `clear` unsets each independently — the same path the
/// dashboard's save form and this MCP tool share (`ops::overlay_params`).
#[tokio::test]
async fn kv_unified_round_trips_through_clear_and_the_command_line() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let created = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create",
            "model_id": "qwen3-pool",
            "gguf_path": "qwen3.gguf",
            "kv_unified": true,
            "kv_unified_per_slot": 16384,
            "parallel": 4,
            "n_predict": 4096
        }),
    )
    .await;
    assert!(!is_error(&created), "create failed: {}", text_of(&created));

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3-pool" }),
        )
        .await,
    );
    assert_eq!(got["params"]["kv_unified"], true);
    assert_eq!(got["params"]["kv_unified_per_slot"], 16384);
    let cmd = got["command_line"].as_str().unwrap();
    assert!(cmd.contains("--kv-unified "), "{cmd}");
    assert!(!cmd.contains("--no-kv-unified"), "{cmd}");
    assert!(cmd.contains("--kv-unified-per-slot 16384"), "{cmd}");

    // Flipping the tri-state to `false` renders the `--no-*` twin, same shape
    // as `reasoning_preserve`.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "qwen3-pool", "kv_unified": false, "clear": "kv_unified_per_slot" }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3-pool" }),
        )
        .await,
    );
    assert_eq!(got["params"]["kv_unified"], false);
    assert!(got["params"]["kv_unified_per_slot"].is_null(), "{got}");
    let cmd = got["command_line"].as_str().unwrap();
    assert!(cmd.contains("--no-kv-unified"), "{cmd}");
    assert!(!cmd.contains("--kv-unified-per-slot"), "{cmd}");

    // `clear: kv_unified` returns the row to llama-server's own default —
    // silent, neither flag rendered.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "qwen3-pool", "clear": "kv_unified" }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "qwen3-pool" }),
        )
        .await,
    );
    assert!(got["params"]["kv_unified"].is_null(), "{got}");
    let cmd = got["command_line"].as_str().unwrap();
    assert!(!cmd.contains("kv-unified"), "{cmd}");
}

/// Decision D2 (unified-KV design §3.3): an explicitly shared pool with more
/// than one effective slot and no `n_predict` has no bound to guard, so the
/// save is refused rather than silently left unguarded — both with an
/// explicit `parallel` and with it left at the auto default (4 slots).
/// Setting `n_predict` is what unblocks it.
#[tokio::test]
async fn kv_unified_save_is_refused_without_max_output_on_a_shared_pool() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let explicit_parallel = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create", "model_id": "no-bound-1", "gguf_path": "m.gguf",
            "kv_unified": true, "parallel": 2
        }),
    )
    .await;
    assert!(
        is_error(&explicit_parallel),
        "{}",
        text_of(&explicit_parallel)
    );
    assert!(
        text_of(&explicit_parallel).contains("n_predict"),
        "{}",
        text_of(&explicit_parallel)
    );

    let auto_slots = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create", "model_id": "no-bound-2", "gguf_path": "m.gguf",
            "kv_unified": true
        }),
    )
    .await;
    assert!(is_error(&auto_slots), "{}", text_of(&auto_slots));

    let accepted = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create", "model_id": "bounded", "gguf_path": "m.gguf",
            // The pool ledger also needs a known pool size (review finding
            // 2) — `ctx_size` here is what makes this fixture "accepted".
            "kv_unified": true, "parallel": 2, "n_predict": 4096, "ctx_size": 16384
        }),
    )
    .await;
    assert!(!is_error(&accepted), "{}", text_of(&accepted));
}

/// A short llama-server alias (`-kvu`, `runtime/argv.rs`'s `SHORT_ALIASES`)
/// typed into freeform `extra_args` must not bypass `validate_kv_unified`
/// (review finding 3): the save has to fold it into the typed `kv_unified`
/// field and judge it exactly like spelling `"kv_unified": true` out, not
/// wait for this row's next load to notice.
#[tokio::test]
async fn a_short_kv_unified_alias_in_extra_args_is_still_validated() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let refused = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create", "model_id": "short-alias-bypass", "gguf_path": "m.gguf",
            "extra_args": "-kvu", "parallel": 2
        }),
    )
    .await;
    assert!(is_error(&refused), "{}", text_of(&refused));
    assert!(
        text_of(&refused).contains("n_predict"),
        "{}",
        text_of(&refused)
    );
}

/// A miss on a model id used to come back as a bare "not found". A caller with
/// no shell then has nowhere to go, so the error enumerates what does exist.
#[tokio::test]
async fn a_missing_local_model_names_the_ones_that_exist() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "create", "model_id": "real-one", "gguf_path": "a.gguf" }),
    )
    .await;

    let miss = call(
        &base,
        &sid,
        "lmgw__local_model_get",
        json!({ "model_id": "typo" }),
    )
    .await;
    assert!(is_error(&miss));
    assert!(text_of(&miss).contains("real-one"), "{}", text_of(&miss));
}

/// Owner overrides (model-capabilities design §7): `capabilities_override`
/// arrives as a JSON string (MCP arguments are flat scalars — mirrors
/// `chat_template_kwargs`), round-trips through `lmgw__local_model_get`, and
/// clears through `clear`. A malformed one — an unknown top-level key, a
/// non-object `capabilities`, invalid JSON — is refused naming the problem
/// rather than silently accepted or dropped.
#[tokio::test]
async fn capabilities_override_round_trips_and_rejects_malformed_shapes() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "create", "model_id": "owner-caps", "gguf_path": "a.gguf" }),
    )
    .await;

    let set = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "owner-caps",
            "capabilities_override": "{\"max_output_tokens\":8192,\"notes\":[\"hand-verified\"]}"
        }),
    )
    .await;
    assert!(!is_error(&set), "{}", text_of(&set));

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "owner-caps" }),
        )
        .await,
    );
    assert_eq!(
        got["capabilities_override"],
        json!({"max_output_tokens": 8192, "notes": ["hand-verified"]})
    );

    // Clearing goes back to null, not an empty object.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "owner-caps", "clear": "capabilities_override" }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "owner-caps" }),
        )
        .await,
    );
    assert!(got["capabilities_override"].is_null(), "{got}");

    // A non-object `capabilities` value is refused, naming the field.
    let bad = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "owner-caps",
            "capabilities_override": "{\"capabilities\": \"not an object\"}"
        }),
    )
    .await;
    assert!(
        is_error(&bad),
        "a non-object capabilities value was accepted"
    );
    assert!(text_of(&bad).contains("capabilities"), "{}", text_of(&bad));

    // Invalid JSON entirely is refused too.
    let bad_json = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "owner-caps",
            "capabilities_override": "{not json"
        }),
    )
    .await;
    assert!(is_error(&bad_json), "invalid JSON was accepted");

    // An unknown top-level key names itself in the error.
    let bad_key = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "owner-caps",
            "capabilities_override": "{\"bogus_key\": 1}"
        }),
    )
    .await;
    assert!(is_error(&bad_key), "an unknown top-level key was accepted");
    assert!(
        text_of(&bad_key).contains("bogus_key"),
        "{}",
        text_of(&bad_key)
    );
}

/// The same owner-override shape, on an alias (`lmgw__model_set`) rather than
/// a local model — the MCP-facing patch carries its own `clear` (the alias
/// patch had none before this field existed).
#[tokio::test]
async fn alias_capabilities_override_round_trips_and_clears() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let up_id = store::insert_upstream(
        &state.db,
        &store::NewUpstream {
            name: "cloud".into(),
            protocol: lmgw_core::config::Protocol::Openai,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__model_set",
            json!({
                "action": "create", "alias": "owner-caps-alias",
                "upstream": up_id.to_string(), "upstream_model": "tgt"
            }),
        )
        .await,
    );
    let id = created["id"].as_i64().unwrap();

    let set = call(
        &base,
        &sid,
        "lmgw__model_set",
        json!({
            "action": "update", "id": id,
            "capabilities_override": "{\"capabilities\":{\"task\":\"chat\",\"endpoints\":[\"/v1/chat/completions\"],\"source\":\"catalog\"}}"
        }),
    )
    .await;
    assert!(!is_error(&set), "{}", text_of(&set));

    let aliases = store::list_aliases(&state.db).await.unwrap();
    let alias = aliases.iter().find(|a| a.id == id).unwrap();
    assert_eq!(
        alias.capabilities_override,
        Some(json!({
            "capabilities": {
                "task": "chat",
                "endpoints": ["/v1/chat/completions"],
                "source": "catalog"
            }
        }))
    );

    call(
        &base,
        &sid,
        "lmgw__model_set",
        json!({ "action": "update", "id": id, "clear": "capabilities_override" }),
    )
    .await;
    let aliases = store::list_aliases(&state.db).await.unwrap();
    let alias = aliases.iter().find(|a| a.id == id).unwrap();
    assert_eq!(alias.capabilities_override, None);

    let bad = call(
        &base,
        &sid,
        "lmgw__model_set",
        json!({ "action": "update", "id": id, "clear": "nonsense_field" }),
    )
    .await;
    assert!(is_error(&bad), "an unknown clear target must be reported");
    assert!(text_of(&bad).contains("nonsense_field"));
}

// ---------------------------------------------------------------------------
// The agent catalog over the tool plane (agent-catalog design §5)
// ---------------------------------------------------------------------------

/// An agent holding the admin token can install an agent — the point of the
/// catalog being data. The read/write split is the usual one: browsing the
/// catalog is a read, installing and deleting are not.
#[tokio::test]
async fn an_agent_can_read_the_catalog_at_read_only_and_install_only_at_full() {
    let manifest = r#"{
      "schema_version": 1,
      "id": "note-taker",
      "name": "Note taker",
      "description": "Files notes into the docs corpora.",
      "model": { "alias": "{{config.model}}" },
      "config": { "schema": { "type": "object", "properties": {
        "model": { "type": "string", "format": "model_alias" }
      }, "required": ["model"] } },
      "tools": [ { "label": "docs" } ],
      "run": { "kind": "chat", "system": "You file notes." }
    }"#;

    // read_only: the two reads are there, the two writes are not.
    let ro = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let ro_sid = initialize(&ro).await;
    let names = list_tool_names(&ro, &ro_sid).await;
    for read in ["lmgw__agents", "lmgw__agent_get"] {
        assert!(names.iter().any(|n| n == read), "missing {read}: {names:?}");
    }
    for write in ["lmgw__agent_set", "lmgw__agent_delete"] {
        assert!(
            !names.iter().any(|n| n == write),
            "{write} leaked into read_only"
        );
    }
    let refused = call(
        &ro,
        &ro_sid,
        "lmgw__agent_set",
        json!({ "manifest": manifest }),
    )
    .await;
    assert!(is_error(&refused), "{refused:?}");
    assert!(
        text_of(&refused).contains("read_only"),
        "{}",
        text_of(&refused)
    );
    // ... and the read really answers, listing the shipped agents.
    let listed = payload(&call(&ro, &ro_sid, "lmgw__agents", json!({})).await);
    assert!(listed["count"].as_u64().unwrap() >= 1, "{listed}");

    // full: validate, install, read back, delete.
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;

    let dry = payload(
        &call(
            &base,
            &sid,
            "lmgw__agent_set",
            json!({ "manifest": manifest, "validate_only": true }),
        )
        .await,
    );
    assert_eq!(dry["ok"], json!(true));
    assert_eq!(dry["validate_only"], json!(true));
    let missing = call(
        &base,
        &sid,
        "lmgw__agent_get",
        json!({ "id": "note-taker" }),
    )
    .await;
    assert!(is_error(&missing), "validate_only wrote something");

    let done = payload(
        &call(
            &base,
            &sid,
            "lmgw__agent_set",
            json!({ "manifest": manifest }),
        )
        .await,
    );
    assert_eq!(done["id"], json!("note-taker"));
    // `docs` is a built-in label, so nothing is missing and the report says so.
    assert_eq!(done["warnings"], json!([]));

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__agent_get",
            json!({ "id": "note-taker" }),
        )
        .await,
    );
    assert_eq!(got["kind"], json!("chat"));
    assert_eq!(got["requires_ok"], json!(true));
    // The manifest comes back as canonical text — a JSON tree would have been
    // through a `BTreeMap` and re-sorted the config form (agents §2.6).
    let stored: Value = serde_json::from_str(got["manifest"].as_str().unwrap()).unwrap();
    assert_eq!(stored["run"]["system"], json!("You file notes."));

    // The same document as an object installs, and says what that cost.
    let as_object: Value = serde_json::from_str(manifest).unwrap();
    let loose = payload(
        &call(
            &base,
            &sid,
            "lmgw__agent_set",
            json!({ "manifest": as_object }),
        )
        .await,
    );
    assert!(
        loose["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("JSON string")),
        "{loose}"
    );
    // ... and an ordering warning is not a missing server: the next step still
    // points at the config form.
    assert!(
        loose["next_step"].as_str().unwrap().contains("agent_get"),
        "{loose}"
    );

    // A manifest that does not validate is a tool error the model can read and
    // fix, not a protocol failure.
    let bad = call(
        &base,
        &sid,
        "lmgw__agent_set",
        json!({ "manifest": manifest.replace("{{config.model}}", "{{config.nope}}") }),
    )
    .await;
    assert!(is_error(&bad));
    assert!(text_of(&bad).contains("config.nope"), "{}", text_of(&bad));

    let gone = payload(
        &call(
            &base,
            &sid,
            "lmgw__agent_delete",
            json!({ "id": "note-taker" }),
        )
        .await,
    );
    assert_eq!(gone["ok"], json!(true));
    assert!(is_error(
        &call(
            &base,
            &sid,
            "lmgw__agent_get",
            json!({ "id": "note-taker" })
        )
        .await
    ));
}

// ---------------------------------------------------------------------------
// The image class through the tool plane (image-generation design §8)
// ---------------------------------------------------------------------------

/// The one departure from the audio precedent the design makes on purpose:
/// image CRUD is an *ops* verb, so `lmgw__image_model_set` and
/// `POST /api/op/image_model_set` are the same function and an agent can bring
/// a pipeline up without the dashboard.
///
/// The fixture is a real models directory with the three files a Z-Image-style
/// pipeline names, because every `files` value is checked against the disk when
/// the row is saved — a row that names a path nothing is at is refused, not
/// stored and discovered at the first start.
const DIT: &str = "Tongyi-MAI/Z-Image-Turbo/z-image-turbo-Q4_K.gguf";
const VAE: &str = "Comfy-Org/flux/ae.safetensors";
const LLM: &str = "Qwen/Qwen3-4B-Instruct-GGUF/Qwen3-4B-Q4_K_M.gguf";

async fn image_state() -> (SharedState, tempfile::TempDir) {
    let state = state_with_mode(SelfAdmin::Full).await;
    let dir = tempfile::tempdir().unwrap();
    for rel in [DIT, VAE, LLM] {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"weights").unwrap();
    }
    let mut s = state.snapshot().settings.clone();
    s.image.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    (state, dir)
}

/// The whole chain an agent drives: create the row from a `files` block, read
/// it back, see it on the model list, change one default, and delete it.
#[tokio::test]
async fn an_agent_creates_reads_updates_and_deletes_an_image_model() {
    let (state, _dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({
                "action": "create",
                "model_id": "z-image-turbo",
                "files": format!("diffusion_model = {DIT}\nvae = {VAE}\nllm = {LLM}"),
                "args": "diffusion_fa\ncfg_scale = 1.0\nsteps = 8",
                "modes": "img_gen",
                "idle_seconds": 120,
            }),
        )
        .await,
    );
    assert_eq!(created["ok"], true, "{created}");
    assert_eq!(created["public_name"], "image/z-image-turbo");
    // A failed vocabulary probe (no container runtime in a test) degrades to
    // the vocabulary lmgw ships with and *says so* — it never blocks the save.
    let warnings = created["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("flags lmgw ships with")),
        "the degraded key check should be reported: {created}"
    );

    let row = state.snapshot().image_models[0].clone();
    assert_eq!(row.files["diffusion_model"], json!(DIT));
    assert_eq!(row.files["vae"], json!(VAE));
    // A bare key is the switch it says it is; a number keeps its type, because
    // `--cfg-scale "1.0"` and `--cfg-scale 1.0` are not the same argv.
    assert_eq!(row.args["diffusion_fa"], json!(true));
    assert_eq!(row.args["cfg_scale"], json!(1.0));
    assert_eq!(row.args["steps"], json!(8));
    assert_eq!(row.idle_seconds, 120);

    // Read back through the shared read tool, which now answers for the class.
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "z-image-turbo", "target": "image" }),
        )
        .await,
    );
    assert_eq!(got["class"], "image");
    assert_eq!(got["public_name"], "image/z-image-turbo");
    assert_eq!(got["edit"], false);
    assert_eq!(got["endpoints"], json!(["/v1/images/generations"]));
    assert_eq!(got["files_present"]["diffusion_model"], true);
    assert_eq!(got["problems"].as_array().map(Vec::len), Some(0), "{got}");
    // The learned transient peak (image-generation §9): nothing has generated
    // on this row, so the tool reports the gap and the one thing that closes
    // it rather than a bare null an agent would read as "zero".
    assert!(got["peak_extra_bytes"].is_null(), "{got}");
    assert!(
        got["peak"]
            .as_str()
            .unwrap_or_default()
            .contains("not learned yet"),
        "{got}"
    );
    // The rendered command line is the thing an owner is really verifying.
    let cmd = got["command_line"].as_str().unwrap();
    assert!(cmd.contains("--eager-load") && cmd.contains(DIT), "{cmd}");

    // And on the model list, under the class prefix.
    let listed = payload(&call(&base, &sid, "lmgw__models", json!({ "kind": "image" })).await);
    let names: Vec<&str> = listed["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["image/z-image-turbo"], "{listed}");
    assert_eq!(listed["models"][0]["kind"], "image");
    assert_eq!(listed["models"][0]["modes"], json!(["img_gen"]));

    // One generation's worth of measurement, as the sampler would have written
    // it — so the update below has something to invalidate.
    let learned = state.snapshot().image_models[0].id;
    lmgw_core::store::set_image_model_peak(&state.db, learned, Some(6 * 1024 * 1024 * 1024))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();

    // Update is partial in the fields and wholesale in the maps: `args`
    // replaces, everything not mentioned stays.
    let updated = payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({
                "action": "update",
                "model_id": "z-image-turbo",
                "args": "steps = 4",
                "edit": true,
            }),
        )
        .await,
    );
    assert_eq!(updated["ok"], true, "{updated}");
    // `steps = 4` is a different pipeline to the GPU, so the measurement is
    // dropped — and the tool says it, because it is a promise admission has
    // stopped making.
    assert_eq!(updated["peak_reset"], true, "{updated}");
    assert!(
        updated["message"]
            .as_str()
            .unwrap_or_default()
            .contains("learned peak reset"),
        "{updated}"
    );
    let row = state.snapshot().image_models[0].clone();
    assert_eq!(row.peak_extra_bytes, None);
    assert_eq!(row.args, json!({"steps": 4}).as_object().cloned().unwrap());
    assert_eq!(row.files.len(), 3, "files must survive an args-only update");
    assert!(row.edit);
    assert_eq!(row.idle_seconds, 120);

    // A path pasted back out of the rendered command line carries the
    // in-container `/models/` prefix; it is dropped rather than stored and
    // rendered twice.
    payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({
                "action": "update",
                "model_id": "z-image-turbo",
                "files": format!("--diffusion-model = /models/{DIT}"),
            }),
        )
        .await,
    );
    let row = state.snapshot().image_models[0].clone();
    assert_eq!(
        row.files,
        json!({"diffusion_model": DIT})
            .as_object()
            .cloned()
            .unwrap()
    );

    let deleted = payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({ "action": "delete", "model_id": "z-image-turbo" }),
        )
        .await,
    );
    assert_eq!(deleted["ok"], true, "{deleted}");
    assert!(state.snapshot().image_models.is_empty());
}

/// The four refusals §4 asks for, each naming what to do about it. All of them
/// happen on the way in: a row that cannot start is never stored.
#[tokio::test]
async fn an_image_row_is_refused_for_the_four_things_that_cannot_start() {
    let (state, _dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let refuse = |args: Value| {
        let base = base.clone();
        let sid = sid.clone();
        async move {
            let r = call(&base, &sid, "lmgw__image_model_set", args).await;
            assert!(is_error(&r), "this should have been refused: {r}");
            text_of(&r)
        }
    };

    // 1. Both ways to load a pipeline at once.
    let msg = refuse(json!({
        "action": "create", "model_id": "both",
        "files": format!("model = {DIT}\ndiffusion_model = {DIT}"),
    }))
    .await;
    assert!(
        msg.contains("alternatives, not a pair"),
        "should name the exclusivity: {msg}"
    );

    // 2. Neither of them.
    let msg = refuse(json!({
        "action": "create", "model_id": "neither", "files": format!("vae = {VAE}"),
    }))
    .await;
    assert!(msg.contains("neither 'model'"), "{msg}");

    // 3. A path nothing is at.
    let msg = refuse(json!({
        "action": "create", "model_id": "missing",
        "files": "diffusion_model = nowhere/weights.gguf",
    }))
    .await;
    assert!(
        msg.contains("nowhere/weights.gguf") && msg.contains("not a file"),
        "should name the missing path: {msg}"
    );

    // 4. A key sd-server does not have — with the suggestion, because an
    //    over-long or truncated key is the common typo.
    let msg = refuse(json!({
        "action": "create", "model_id": "typo",
        "files": format!("diffusion_model = {DIT}"),
        "args": "vae_tiling_x",
    }))
    .await;
    assert!(
        msg.contains("vae_tiling_x") && msg.contains("did you mean") && msg.contains("vae-tiling"),
        "should suggest the real flag: {msg}"
    );

    assert!(
        state.snapshot().image_models.is_empty(),
        "no refused row may have been stored"
    );
}

/// `clear` names **whole fields**, not substrings. `clear=extra_run_args`
/// used to empty `args` as well (`"extra_run_args".contains("args")`), which
/// silently threw away every generation default a row had — and the row it
/// left behind still saved.
#[tokio::test]
async fn clear_names_a_whole_field_and_never_a_substring_of_one() {
    let (state, _dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let create = |args: Value| {
        let base = base.clone();
        let sid = sid.clone();
        async move { payload(&call(&base, &sid, "lmgw__image_model_set", args).await) }
    };

    let r = create(json!({
        "action": "create", "model_id": "z",
        "files": format!("diffusion_model = {DIT}"),
        "args": "steps = 8\ncfg_scale = 1.0",
        "extra_run_args": "--device nvidia.com/gpu=all",
    }))
    .await;
    assert_eq!(r["ok"], true, "{r}");

    // Clearing the run args leaves the generation defaults exactly as they
    // were — two fields, two names.
    let r = create(json!({
        "action": "update", "model_id": "z", "clear": "extra_run_args",
    }))
    .await;
    assert_eq!(r["ok"], true, "{r}");
    let row = state.snapshot().image_models[0].clone();
    assert_eq!(row.extra_run_args, None);
    assert_eq!(
        row.args["steps"],
        json!(8),
        "args must survive: {:?}",
        row.args
    );
    assert_eq!(row.args["cfg_scale"], json!(1.0));

    // And naming `args` really does clear them.
    let r = create(json!({ "action": "update", "model_id": "z", "clear": "args" })).await;
    assert_eq!(r["ok"], true, "{r}");
    assert!(state.snapshot().image_models[0].args.is_empty());

    // `files` is not clearable — a row with no files names no pipeline — and
    // a misspelled field name is refused rather than silently ignored.
    for (clear, needle) in [
        ("files", "not clearable"),
        ("agrs", "not a clearable field name"),
    ] {
        let r = call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({ "action": "update", "model_id": "z", "clear": clear }),
        )
        .await;
        assert!(is_error(&r), "clear={clear} should be refused: {r}");
        assert!(
            text_of(&r).contains(needle),
            "clear={clear}: {}",
            text_of(&r)
        );
    }
    assert_eq!(state.snapshot().image_models[0].files.len(), 1);
}

/// Four things a row can say that would render an argv nobody meant, each
/// refused at save time with the sentence that fixes it.
#[tokio::test]
async fn an_image_row_is_refused_for_the_keys_that_render_wrong() {
    let (state, _dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let refuse = |args: Value| {
        let base = base.clone();
        let sid = sid.clone();
        async move {
            let r = call(&base, &sid, "lmgw__image_model_set", args).await;
            assert!(is_error(&r), "this should have been refused: {r}");
            text_of(&r)
        }
    };

    // 1. A path in `args` is passed through verbatim, so it names a host path
    //    the container cannot see — `files` is the map that gets mounted.
    let msg = refuse(json!({
        "action": "create", "model_id": "path-in-args",
        "files": format!("diffusion_model = {DIT}"),
        "args": format!("vae = {VAE}"),
    }))
    .await;
    assert!(
        msg.contains("takes a path") && msg.contains("Move it to files"),
        "{msg}"
    );

    // 2. An alias of a flag lmgw renders itself: `l` *is* `--listen-ip`, and
    //    reading the raw key let it through to render a second copy.
    let msg = refuse(json!({
        "action": "create", "model_id": "alias",
        "files": format!("diffusion_model = {DIT}"),
        "args": "l = 127.0.0.1",
    }))
    .await;
    assert!(
        msg.contains("--listen-ip") && msg.contains("lmgw renders itself"),
        "{msg}"
    );

    // 3. `m` is `--model`, so a row spelling it that way beside a
    //    diffusion_model names both ways to load a pipeline.
    let msg = refuse(json!({
        "action": "create", "model_id": "both-by-alias",
        "files": format!("m = {DIT}\ndiffusion_model = {DIT}"),
    }))
    .await;
    assert!(msg.contains("alternatives, not a pair"), "{msg}");

    // 4. A directory value that climbs out of the models dir — this one is
    //    resolved and created on the *host*, so it never gets to be relative
    //    to something else.
    let msg = refuse(json!({
        "action": "create", "model_id": "escape",
        "files": format!("diffusion_model = {DIT}\nlora_model_dir = ../../tmp/x"),
    }))
    .await;
    assert!(
        msg.contains("../../tmp/x") && msg.contains("plain path relative to the image models dir"),
        "{msg}"
    );

    assert!(
        state.snapshot().image_models.is_empty(),
        "no refused row may have been stored"
    );
}

/// A quoted scalar in the text form is the string it quotes — the quotes are
/// JSON syntax, not two bytes of the value. Stored with them, `cfg_scale`
/// rendered as `--cfg-scale "1.0"` and sd-server could not parse it.
#[tokio::test]
async fn a_quoted_value_in_the_text_form_keeps_its_value_not_its_quotes() {
    let (state, _dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let r = payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({
                "action": "create", "model_id": "z",
                "files": format!("diffusion_model = {DIT}"),
                "args": "cfg_scale = \"1.0\"\nsampling_method = \"euler\"\ntype = q8_0",
            }),
        )
        .await,
    );
    assert_eq!(r["ok"], true, "{r}");
    let row = state.snapshot().image_models[0].clone();
    assert_eq!(row.args["cfg_scale"], json!("1.0"));
    assert_eq!(row.args["sampling_method"], json!("euler"));
    assert_eq!(row.args["type"], json!("q8_0"));
    let cmd = r["command_line"].as_str().unwrap();
    assert!(cmd.contains("--cfg-scale 1.0"), "{cmd}");
    assert!(
        !cmd.contains("\\\""),
        "no quote bytes reach the argv: {cmd}"
    );
}

/// `lmgw__llama_flags` reads *llama-server's* vocabulary. Handed an image
/// model id it used to take the id for an image reference and try to pull it;
/// now it says which verb answers for that class.
#[tokio::test]
async fn llama_flags_refuses_an_image_model_by_name() {
    let (state, _dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({
                "action": "create", "model_id": "z-image-turbo",
                "files": format!("diffusion_model = {DIT}"),
            }),
        )
        .await,
    );

    let r = call(
        &base,
        &sid,
        "lmgw__llama_flags",
        json!({ "model": "z-image-turbo" }),
    )
    .await;
    assert!(is_error(&r), "{r}");
    let msg = text_of(&r);
    assert!(msg.contains("is an image"), "{msg}");
    assert!(msg.contains("target=image"), "{msg}");
    let _ = state;
}

/// Without a models directory there is nothing for a relative path to be
/// relative *to*, so the refusal names the setting rather than the file.
#[tokio::test]
async fn an_image_row_with_no_models_dir_refuses_by_naming_the_setting() {
    let state = state_with_mode(SelfAdmin::Full).await;
    assert!(state.snapshot().settings.image.models_dir.is_empty());
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let r = call(
        &base,
        &sid,
        "lmgw__image_model_set",
        json!({
            "action": "create", "model_id": "z",
            "files": "diffusion_model = a/b.gguf",
        }),
    )
    .await;
    assert!(is_error(&r), "{r}");
    let msg = text_of(&r);
    assert!(
        msg.contains("image models directory is not configured") && msg.contains("models_dir"),
        "{msg}"
    );
    assert!(state.snapshot().image_models.is_empty());
}

/// `writes: true`, like every other `*_set`: hidden at `read_only` and refused
/// by name with the setting that would allow it.
#[tokio::test]
async fn image_model_set_is_gated_at_full_like_every_other_mutation() {
    let base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let sid = initialize(&base).await;
    let names = list_tool_names(&base, &sid).await;
    assert!(
        !names.iter().any(|n| n == "lmgw__image_model_set"),
        "a write tool must not be listed at read_only: {names:?}"
    );

    let r = call(
        &base,
        &sid,
        "lmgw__image_model_set",
        json!({ "action": "delete", "model_id": "whatever" }),
    )
    .await;
    assert!(is_error(&r));
    let msg = text_of(&r);
    assert!(msg.contains("read_only") && msg.contains("full"), "{msg}");
}

/// The two read tools that answer for the class without being able to *plan*
/// it: `local_model_check` runs the same pre-flight group apply runs, and
/// `local_model_plan` matches the file against the shipped recipes rather than
/// inventing a plan — this fixture's files belong to no recipe, so the answer
/// is the honest "unknown family" with the keys it does know.
#[tokio::test]
async fn check_reports_an_image_rows_problems_and_plan_names_the_recipes_it_knows() {
    let (state, dir) = image_state().await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    payload(
        &call(
            &base,
            &sid,
            "lmgw__image_model_set",
            json!({
                "action": "create", "model_id": "z",
                "files": format!("diffusion_model = {DIT}\nvae = {VAE}"),
            }),
        )
        .await,
    );
    // Clean while the files are there…
    let ok = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_check",
            json!({ "model_id": "z", "target": "image" }),
        )
        .await,
    );
    assert_eq!(ok["checked"], 1, "{ok}");
    assert_eq!(ok["broken"], 0, "{ok}");
    assert_eq!(ok["models"][0]["class"], "image");
    assert_eq!(ok["models"][0]["modes"], json!(["img_gen"]));

    // …and broken the moment one of them is gone, which is exactly the state a
    // save could not have refused because it happened afterwards.
    std::fs::remove_file(dir.path().join(VAE)).unwrap();
    let broken = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_check",
            json!({ "target": "image" }),
        )
        .await,
    );
    assert_eq!(broken["broken"], 1, "{broken}");
    let issues = broken["models"][0]["problems"][0].as_str().unwrap();
    assert!(
        issues.contains("vae") && issues.contains("missing"),
        "{issues}"
    );

    let plan = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_plan",
            json!({ "gguf_path": DIT, "target": "image" }),
        )
        .await,
    );
    assert_eq!(plan["class"], "image");
    assert_eq!(plan["planned"], false);
    assert!(
        plan["reason"].as_str().unwrap().contains("unknown family"),
        "{plan}"
    );
    let known: Vec<&str> = plan["known_recipes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert!(known.contains(&"z-image-turbo"), "{known:?}");
    assert!(
        plan["next_step"]
            .as_str()
            .unwrap()
            .contains("lmgw__image_model_set"),
        "{plan}"
    );
}

/// The recipe chain over the wire, end to end: `lmgw__image_recipes` to see
/// what is shippable, `lmgw__image_recipe_add` to get the prefilled row, that
/// row straight into `lmgw__image_model_set action=create`, and the row it
/// makes checked and planned back.
///
/// The fixture puts the recipe's real files on disk, so nothing is queued and
/// no hub is needed here — the queueing half owns the process-wide
/// `HF_ENDPOINT` override and therefore lives in `tests/it/image_recipes.rs`,
/// which is single-purpose for exactly that reason. What this covers is the
/// *wire*: that the two tools exist on the admin plane, that the row comes
/// back in the field names `lmgw__image_model_set` takes, and that handing it
/// straight over works without an agent editing anything.
#[tokio::test]
async fn an_agent_brings_up_a_pipeline_from_a_recipe_over_the_wire() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let dir = tempfile::tempdir().unwrap();
    let recipe = lmgw_core::image_recipes::find("z-image-turbo").unwrap();
    for c in recipe.components {
        let p = dir.path().join(c.dest_rel_path());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"weights").unwrap();
    }
    let mut settings = state.snapshot().settings.clone();
    settings.image.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    // 1. The list, with this box's state folded in.
    let list = payload(&call(&base, &sid, "lmgw__image_recipes", json!({})).await);
    let z = list["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "z-image-turbo")
        .expect("z-image-turbo is shipped")
        .clone();
    assert_eq!(z["installed"], true, "{z}");
    assert_eq!(z["served"], false);
    assert_eq!(z["components"].as_array().unwrap().len(), 3);
    assert!(
        z["vram_note"].as_str().unwrap().contains("7.1 GiB"),
        "the measured figure travels with the recipe: {z}"
    );

    // 2. Add: nothing to queue, and a row in image_model_set's field names.
    let added = payload(
        &call(
            &base,
            &sid,
            "lmgw__image_recipe_add",
            json!({ "key": "z-image-turbo" }),
        )
        .await,
    );
    assert_eq!(added["files_queued"], 0, "{added}");
    assert_eq!(added["already_present"].as_array().unwrap().len(), 3);
    let row = added["row"].clone();
    assert_eq!(row["action"], "create");
    assert!(
        state.snapshot().image_models.is_empty(),
        "the recipe hands over a row, it does not create one"
    );

    // 3. That row, unedited, through the CRUD tool. `files` and `args` arrive
    //    as JSON objects here — the patch takes both spellings (§14, WP4).
    let created = payload(&call(&base, &sid, "lmgw__image_model_set", row.clone()).await);
    assert_eq!(created["ok"], true, "{created}");
    assert_eq!(created["public_name"], "image/z-image-turbo");
    let stored = state.snapshot().image_models[0].clone();
    assert_eq!(
        stored.files["diffusion_model"],
        json!("leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf")
    );
    assert_eq!(stored.args["steps"], json!(8));
    assert_eq!(stored.args["cfg_scale"], json!(1.0));
    assert_eq!(stored.args["diffusion_fa"], json!(true));
    assert_eq!(stored.modes, vec!["img_gen".to_string()]);

    // 4. It passes the class's own pre-flight…
    let checked = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_check",
            json!({ "model_id": "z-image-turbo", "target": "image" }),
        )
        .await,
    );
    assert_eq!(checked["broken"], 0, "{checked}");

    // 5. …and planning its diffusion model back returns the same recipe, now
    //    complete, so the loop closes.
    let plan = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_plan",
            json!({
                "gguf_path": "leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf",
                "target": "image",
            }),
        )
        .await,
    );
    assert_eq!(plan["planned"], true, "{plan}");
    assert_eq!(plan["recipe"], "z-image-turbo");
    assert_eq!(plan["complete"], true);
    assert_eq!(plan["params"]["files"], row["files"]);

    // …and the recipe now reports itself served.
    let list = payload(&call(&base, &sid, "lmgw__image_recipes", json!({})).await);
    let z = list["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "z-image-turbo")
        .unwrap()
        .clone();
    assert_eq!(z["served"], true, "{z}");

    // An unknown key is refused with the list of what is known.
    let bad = call(
        &base,
        &sid,
        "lmgw__image_recipe_add",
        json!({ "key": "stable-cascade" }),
    )
    .await;
    assert!(is_error(&bad));
    assert!(text_of(&bad).contains("flux1-schnell"), "{}", text_of(&bad));
}
