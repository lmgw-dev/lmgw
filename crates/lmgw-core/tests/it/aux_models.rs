//! The aux class through the tool plane: embedding / rerank models created,
//! planned, listed, read back and load-tested without ever touching the chat
//! table or the chat models dir.
//!
//! The scenario this guards is a real one. An agent asked to compare
//! embedding models on the CPU found that the self-admin tools could not
//! create aux rows and that every file tool only saw the chat models dir — so
//! it hard-linked the GGUFs into the chat tree, created chat-class rows named
//! `embed/…` with `--embedding` in their args, and then watched the load test
//! report every one of them broken, because the test asked an encoder to
//! generate a token. Each of those three dead ends has a test here.
//!
//! Runs against a fake `CommandRunner` (no podman) and one wiremock server
//! standing in for every container, the same way `ops_container.rs` does.

use std::sync::{Arc, Mutex};

use lmgw_core::config::{AuxKind, SelfAdmin, Settings};
use lmgw_core::gguf::synth;
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAuxModel, NewLocalModel};
use lmgw_core::{modelinfo, ops};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Fake podman + one mock server every container "is"
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    /// When set, a throwaway `--help` run fails the way ik's binary does
    /// without the GPU attached, instead of printing the real vocabulary.
    help_broken: std::sync::atomic::AtomicBool,
}

/// A real `llama-server --help` (the official CUDA image at 4b1a27fa0): what
/// every row's extra args are validated against when they are saved.
const LLAMA_HELP: &str = include_str!("../fixtures/llama/llama-server-help-4b1a27fa0.txt");

fn ok(stdout: &str) -> CmdOutput {
    CmdOutput {
        status: 0,
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

#[async_trait::async_trait]
impl CommandRunner for Fake {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "podman");
        self.calls.lock().unwrap().push(args.to_vec());
        match args[0].as_str() {
            "run" if args.last().is_some_and(|a| a == "--help") => {
                if self.help_broken.load(std::sync::atomic::Ordering::Relaxed) {
                    Ok(CmdOutput {
                        status: 127,
                        stdout: String::new(),
                        stderr: "/llama-server: error while loading shared libraries: \
                                 libcuda.so.1: cannot open shared object file"
                            .into(),
                    })
                } else {
                    Ok(ok(LLAMA_HELP))
                }
            }
            "run" => Ok(ok("c0ffee\n")),
            _ => Ok(ok("")),
        }
    }
}

/// A container that answers healthy and serves the three probe endpoints
/// (under `/v1`, where a held route's endpoint points). `embedding` is what
/// `/v1/embeddings` returns; `/v1/rerank` always scores both documents;
/// `/v1/chat/completions` always yields a token.
async fn container(embedding: Vec<f64>) -> MockServer {
    let server = MockServer::start().await;
    for p in ["/health", "/v1/models"] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": embedding }],
            "usage": { "prompt_tokens": 3, "total_tokens": 3 }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "index": 0, "relevance_score": 0.91 },
                { "index": 1, "relevance_score": 0.02 }
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hi" } }]
        })))
        .mount(&server)
        .await;
    server
}

struct Fixture {
    state: SharedState,
    podman: Arc<Fake>,
    chat_dir: tempfile::TempDir,
    /// Kept alive for the test's duration — the settings point at it.
    _aux_dir: tempfile::TempDir,
    server: MockServer,
}

const EMBEDDER: &str = "Qwen/Qwen3-Embedding-0.6B-GGUF/Qwen3-Embedding-0.6B-Q8_0.gguf";
const RERANKER: &str = "BAAI/bge-reranker-v2-m3-GGUF/bge-reranker-v2-m3-Q8_0.gguf";
const CHAT: &str = "unsloth/Qwen3.5-9B-GGUF/Qwen3.5-9B-UD-Q4_K_XL.gguf";
/// The credential `/mcp/admin` accepts: the plaintext of the enabled
/// `owner:self-admin` key the fixture installs (principals §3.7).
const ADMIN_TOKEN: &str = "aux-test-token";

/// Two separate models dirs — the point — with an embedder and a reranker in
/// the aux one and a chat model in the chat one, self-admin at `full`, and a
/// runtime whose every start lands on `server`.
async fn fixture(embedding: Vec<f64>) -> Fixture {
    let state = AppState::init_for_tests().await.unwrap();
    let chat_dir = tempfile::tempdir().unwrap();
    let aux_dir = tempfile::tempdir().unwrap();
    synth::embedding("qwen3", 3, 32768).write_to(&aux_dir.path().join(EMBEDDER));
    let mut rr = synth::embedding("bert", 4, 8192);
    rr.tensor("cls.output.weight");
    rr.write_to(&aux_dir.path().join(RERANKER));
    synth::chat("qwen35", 262144).write_to(&chat_dir.path().join(CHAT));

    let server = container(embedding).await;
    let port = server.address().port();
    let podman = Arc::new(Fake::default());
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        podman.clone(),
        reqwest::Client::new(),
        Arc::new(move || Ok(port)),
    )));

    let mut s = Settings {
        self_admin: SelfAdmin::Full,
        ..Settings::default()
    };
    s.router.models_dir = chat_dir.path().display().to_string();
    s.aux_router.models_dir = aux_dir.path().display().to_string();
    s.vram.load_timeout_seconds = 5;
    s.vram.unload_timeout_seconds = 5;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    lmgw_core::agents::token::set_owner_key(
        &state,
        lmgw_core::agents::token::OWNER_SELF_ADMIN,
        ADMIN_TOKEN,
        true,
    )
    .await
    .unwrap();

    Fixture {
        state,
        podman,
        chat_dir,
        _aux_dir: aux_dir,
        server,
    }
}

impl Fixture {
    fn verb(&self, v: &str) -> Vec<Vec<String>> {
        self.podman
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == v)
            .cloned()
            .collect()
    }
}

fn aux_row(model_id: &str, gguf: &str, kind: AuxKind, pooling: Option<&str>) -> NewAuxModel {
    NewAuxModel {
        model_id: model_id.into(),
        gguf_path: gguf.into(),
        kind,
        pooling: pooling.map(String::from),
        ctx_size: None,
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

// ---------------------------------------------------------------------------
// MCP wire helpers (the same three calls an agent makes)
// ---------------------------------------------------------------------------

async fn serve(state: SharedState) -> String {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

async fn rpc(base: &str, session: Option<&str>, body: Value) -> (Option<String>, Value) {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/mcp/admin"))
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {ADMIN_TOKEN}"))
        .json(&body);
    if let Some(sid) = session {
        req = req.header("mcp-session-id", sid);
    }
    let resp = req.send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let text = resp.text().await.unwrap_or_default();
    (sid, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn initialize(base: &str) -> String {
    let (sid, _) = rpc(
        base,
        None,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                        "clientInfo": { "name": "aux-test", "version": "0" } }
        }),
    )
    .await;
    sid.expect("session id")
}

async fn tool_names(base: &str, sid: &str) -> Vec<String> {
    let (_, body) = rpc(
        base,
        Some(sid),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// Call a tool; returns `(is_error, payload-or-text)`.
async fn call(base: &str, sid: &str, name: &str, args: Value) -> (bool, Value) {
    let (_, body) = rpc(
        base,
        Some(sid),
        json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        }),
    )
    .await;
    assert!(body["error"].is_null(), "JSON-RPC error for {name}: {body}");
    let result = &body["result"];
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let is_err = result["isError"] == Value::Bool(true);
    (
        is_err,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The tool exists, and is gated like every other mutation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn aux_model_set_is_listed_at_full_and_hidden_at_read_only() {
    let f = fixture(vec![0.1, 0.2]).await;
    let base = serve(f.state.clone()).await;
    let sid = initialize(&base).await;
    let names = tool_names(&base, &sid).await;
    assert!(
        names.iter().any(|n| n == "lmgw__aux_model_set"),
        "{names:?}"
    );

    let mut s = f.state.snapshot().settings.clone();
    s.self_admin = SelfAdmin::ReadOnly;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    let sid = initialize(&base).await;
    let names = tool_names(&base, &sid).await;
    assert!(
        !names.iter().any(|n| n == "lmgw__aux_model_set"),
        "{names:?}"
    );
    let (is_err, msg) = call(
        &base,
        &sid,
        "lmgw__aux_model_set",
        json!({ "action": "delete" }),
    )
    .await;
    assert!(is_err && text(&msg).contains("read_only"), "{msg}");
}

// ---------------------------------------------------------------------------
// Discover → plan → create → read back, all against the aux dir
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_embedding_model_is_planned_created_and_read_back_as_an_aux_row() {
    let f = fixture(vec![0.1, 0.2]).await;
    let base = serve(f.state.clone()).await;
    let sid = initialize(&base).await;

    // Listing: the chat listing does not see the aux dir, and vice versa —
    // that is the fact the old workaround was built around, said out loud.
    let (_, chat_files) = call(&base, &sid, "lmgw__gguf_files", json!({})).await;
    assert_eq!(chat_files["target"], "chat");
    assert_eq!(chat_files["count"], 1, "{chat_files}");
    let (_, aux_files) = call(&base, &sid, "lmgw__gguf_files", json!({ "target": "aux" })).await;
    assert_eq!(aux_files["target"], "aux");
    let paths: Vec<&str> = aux_files["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&EMBEDDER) && paths.contains(&RERANKER),
        "{paths:?}"
    );

    // Inspect says which class serves the file, from its header.
    let (_, inspect) = call(
        &base,
        &sid,
        "lmgw__model_inspect",
        json!({ "gguf_path": EMBEDDER, "target": "aux", "probe": false }),
    )
    .await;
    assert_eq!(inspect["serve_as"], "aux", "{inspect}");
    assert_eq!(inspect["aux_kind"], "embed");
    assert_eq!(inspect["pooling_type"], "last");

    // The plan carries lmgw__aux_model_set's parameters, read from the header.
    let (_, plan) = call(
        &base,
        &sid,
        "lmgw__local_model_plan",
        json!({ "gguf_path": EMBEDDER, "target": "aux", "probe": false }),
    )
    .await;
    assert_eq!(plan["class"], "aux", "{plan}");
    assert_eq!(plan["apply_ready"], true);
    assert_eq!(plan["params"]["kind"], "embed");
    assert_eq!(plan["params"]["pooling"], "last");
    assert_eq!(plan["params"]["ctx_size"], 32768);
    assert_eq!(plan["model_id"], "qwen3-embedding-0.6b");
    assert!(
        plan["next_step"]
            .as_str()
            .unwrap()
            .contains("lmgw__aux_model_set"),
        "{plan}"
    );

    // Create from the plan, with CPU placement in the escape hatch.
    let (is_err, created) = call(
        &base,
        &sid,
        "lmgw__aux_model_set",
        json!({
            "action": "create",
            "model_id": "qwen3-embedding-0.6b-cpu",
            "gguf_path": EMBEDDER,
            "kind": "embed",
            "pooling": "last",
            "ctx_size": 32768,
            "extra_args": "--n-gpu-layers 0\n--threads 16",
        }),
    )
    .await;
    assert!(!is_err, "{created}");
    assert_eq!(created["public_name"], "embed/qwen3-embedding-0.6b-cpu");
    assert!(
        created["warnings"].as_array().unwrap().is_empty(),
        "a plan-derived row must not warn: {created}"
    );

    let snap = f.state.snapshot();
    let row = snap
        .aux_models
        .iter()
        .find(|m| m.model_id == "qwen3-embedding-0.6b-cpu")
        .expect("row in the live snapshot");
    assert_eq!(row.kind, AuxKind::Embed);
    assert_eq!(row.pooling.as_deref(), Some("last"));
    assert_eq!(row.ctx_size, Some(32768));
    assert_eq!(row.args, vec!["--n-gpu-layers", "0", "--threads", "16"]);
    assert!(
        snap.local_models.is_empty(),
        "nothing landed in the chat table"
    );

    // Read back by model id with no target: found in the aux table.
    let (_, got) = call(
        &base,
        &sid,
        "lmgw__local_model_get",
        json!({ "model_id": "qwen3-embedding-0.6b-cpu" }),
    )
    .await;
    assert_eq!(got["class"], "aux", "{got}");
    assert_eq!(got["kind"], "embed");
    assert_eq!(got["endpoint"], "/v1/embeddings");
    assert_eq!(got["gguf_present"], true);
    assert!(got["problems"].as_array().unwrap().is_empty(), "{got}");
    let cmd = got["command_line"].as_str().unwrap();
    assert!(
        cmd.contains("--embeddings") && cmd.contains("--pooling last"),
        "{cmd}"
    );
    assert!(cmd.contains("--n-gpu-layers 0"), "{cmd}");

    // The listing now shows the file in use.
    let (_, aux_files) = call(&base, &sid, "lmgw__gguf_files", json!({ "target": "aux" })).await;
    let used = aux_files["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == EMBEDDER)
        .map(|f| f["used_by"].clone())
        .unwrap();
    assert_eq!(used, json!(["qwen3-embedding-0.6b-cpu"]));

    // The VRAM planner charges the CPU-only row nothing.
    let fp = f
        .state
        .vram
        .context_tokens(
            &snap,
            lmgw_core::runtime::Class::Aux,
            "qwen3-embedding-0.6b-cpu",
        )
        .await;
    assert_eq!(fp, Some(32768));
}

/// A row whose image's `--help` could not be read is still saved — the check
/// is advisory — but it says its flags went unchecked, and why. It used to
/// say nothing at all, which read exactly like a config with nothing wrong in
/// it (every ik image, whose binary was on no path the read tried).
#[tokio::test]
async fn a_row_whose_flags_could_not_be_checked_says_so() {
    use std::sync::atomic::Ordering::Relaxed;
    let f = fixture(vec![0.1, 0.2]).await;
    f.podman.help_broken.store(true, Relaxed);

    let chat = ops::local_model_set(
        &f.state,
        serde_json::from_value(json!({
            "action": "create", "model_id": "q", "gguf_path": CHAT,
            "extra_args": "--no-warmup",
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    let said = chat["warnings"].to_string();
    for part in [
        "could not read llama-server --help from image",
        "libcuda.so.1",
        "not validated",
    ] {
        assert!(said.contains(part), "{part:?} missing: {chat}");
    }
    let got = crate::common::read_wire(
        ops::local_model_get(&f.state, None, Some("q"), None)
            .await
            .unwrap(),
    );
    assert!(
        got["problems"].to_string().contains("not validated"),
        "{got}"
    );

    let aux = ops::aux_model_set(
        &f.state,
        serde_json::from_value(json!({
            "action": "create", "model_id": "e", "gguf_path": EMBEDDER,
            "kind": "embed", "pooling": "last",
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    assert!(
        aux["warnings"].to_string().contains("not validated"),
        "{aux}"
    );

    // Once the image answers, the same rows validate clean: the warning was
    // about the read, not the row.
    f.podman.help_broken.store(false, Relaxed);
    for (model_id, target) in [("q", None), ("e", Some("aux"))] {
        let got = crate::common::read_wire(
            ops::local_model_get(&f.state, None, Some(model_id), target)
                .await
                .unwrap(),
        );
        assert_eq!(got["problems"], json!([]), "{got}");
    }
}

#[tokio::test]
async fn a_reranker_is_recognised_by_its_head_and_planned_without_pooling() {
    let f = fixture(vec![0.1]).await;
    let plan = modelinfo::local_model_plan(&f.state, RERANKER, false, Some("aux"))
        .await
        .unwrap();
    assert_eq!(plan["class"], "aux", "{plan}");
    assert_eq!(plan["params"]["kind"], "rerank");
    assert!(plan["params"].get("pooling").is_none(), "{plan}");

    // Creating it as an embedder is caught by the header check.
    let created = ops::aux_model_set(
        &f.state,
        ops::patch_from_args(Some(
            json!({ "action": "create", "model_id": "rr", "gguf_path": RERANKER, "kind": "embed" })
                .as_object()
                .unwrap()
                .clone(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
    let warnings = created["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("kind=rerank")),
        "{warnings:?}"
    );

    // And pooling on a reranker is refused outright.
    let err = ops::aux_model_set(
        &f.state,
        ops::patch_from_args(Some(
            json!({ "action": "update", "model_id": "rr", "kind": "rerank", "pooling": "mean" })
                .as_object()
                .unwrap()
                .clone(),
        ))
        .unwrap(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("rerank models take no pooling"), "{err}");
}

// ---------------------------------------------------------------------------
// The wrong directory is named, not worked around
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_embedder_found_in_the_chat_dir_gets_an_aux_plan_that_says_where_it_belongs() {
    let f = fixture(vec![0.1]).await;
    // The file "arrived" in the chat dir (a chat-target download).
    synth::embedding("qwen3", 3, 32768).write_to(&f.chat_dir.path().join(EMBEDDER));

    let plan = modelinfo::local_model_plan(&f.state, EMBEDDER, false, None)
        .await
        .unwrap();
    assert_eq!(
        plan["class"], "aux",
        "an encoder is never planned as chat: {plan}"
    );
    assert_eq!(plan["target"], "chat");
    assert_eq!(plan["apply_ready"], false);
    let warnings = plan["warnings"].to_string();
    assert!(warnings.contains("aux models dir"), "{warnings}");
    assert!(warnings.contains("target=aux"), "{warnings}");
    assert!(
        warnings.contains("Do not hard-link or copy"),
        "the trap has to be named: {warnings}"
    );

    // An aux row cannot address the chat tree at all.
    let err = ops::aux_model_set(
        &f.state,
        ops::patch_from_args(Some(
            json!({ "action": "create", "model_id": "x", "gguf_path": "unsloth/nope.gguf" })
                .as_object()
                .unwrap()
                .clone(),
        ))
        .unwrap(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("aux models directory"), "{err}");
    assert!(err.contains("target=aux"), "{err}");
}

// ---------------------------------------------------------------------------
// The load test asks each class for what it can do
// ---------------------------------------------------------------------------

#[tokio::test]
async fn load_testing_an_embedding_model_embeds_instead_of_generating() {
    let f = fixture(vec![0.25, -0.5, 0.75]).await;
    store::insert_aux_model(
        &f.state.db,
        &aux_row("e", EMBEDDER, AuxKind::Embed, Some("last")),
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();

    let out = crate::common::model_test_wire(
        modelinfo::local_model_test(&f.state, "e", None)
            .await
            .unwrap(),
    );
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["class"], "aux");
    assert_eq!(out["probe"], "embed");
    assert_eq!(out["dimensions"], 3);
    assert_eq!(f.verb("run").len(), 1, "the container was started for real");

    let hits = f.server.received_requests().await.unwrap();
    assert!(
        hits.iter().any(|r| r.url.path() == "/v1/embeddings"),
        "no embeddings call"
    );
    assert!(
        !hits.iter().any(|r| r.url.path() == "/v1/chat/completions"),
        "an encoder must never be asked to generate"
    );
}

#[tokio::test]
async fn an_all_zero_vector_fails_the_load_test_with_a_hint() {
    let f = fixture(vec![0.0, 0.0, 0.0, 0.0]).await;
    store::insert_aux_model(
        &f.state.db,
        &aux_row("z", EMBEDDER, AuxKind::Embed, Some("last")),
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();

    let out = crate::common::model_test_wire(
        modelinfo::local_model_test(&f.state, "z", Some("aux"))
            .await
            .unwrap(),
    );
    assert_eq!(out["ok"], false, "{out}");
    assert!(out["error"].as_str().unwrap().contains("all-zero"), "{out}");
    assert!(out["hint"].as_str().unwrap().contains("pooling"), "{out}");
}

#[tokio::test]
async fn load_testing_a_reranker_scores_documents() {
    let f = fixture(vec![0.1]).await;
    store::insert_aux_model(&f.state.db, &aux_row("rr", RERANKER, AuxKind::Rerank, None))
        .await
        .unwrap();
    f.state.reload_snapshot().await.unwrap();

    let out = crate::common::model_test_wire(
        modelinfo::local_model_test(&f.state, "rr", None)
            .await
            .unwrap(),
    );
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["probe"], "rerank");
    assert_eq!(out["scored"], 2);
    let hits = f.server.received_requests().await.unwrap();
    assert!(hits.iter().any(|r| r.url.path() == "/v1/rerank"));
}

/// The old workaround's shape: a chat-class row whose args turn the server
/// into an embedder. It still exists on real gateways, and judging it by
/// generation is exactly the false "broken" the report complained about.
#[tokio::test]
async fn a_chat_row_with_embedding_args_is_probed_for_embeddings() {
    let f = fixture(vec![0.3, 0.4]).await;
    synth::embedding("qwen3", 3, 32768).write_to(&f.chat_dir.path().join("hack/e.gguf"));
    store::insert_local_model(
        &f.state.db,
        &NewLocalModel {
            model_id: "embed/e-cpu".into(),
            gguf_path: "hack/e.gguf".into(),
            params: Default::default(),
            args: vec!["--embedding".into(), "--pooling".into(), "last".into()],
            idle_seconds: 0,
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
    f.state.reload_snapshot().await.unwrap();

    let out = crate::common::model_test_wire(
        modelinfo::local_model_test(&f.state, "embed/e-cpu", None)
            .await
            .unwrap(),
    );
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["class"], "chat");
    assert_eq!(out["probe"], "embed");
    assert_eq!(out["dimensions"], 2);
}

#[tokio::test]
async fn a_plain_chat_model_is_still_tested_by_generation() {
    let f = fixture(vec![0.1]).await;
    store::insert_local_model(
        &f.state.db,
        &NewLocalModel {
            model_id: "chat".into(),
            gguf_path: CHAT.into(),
            params: Default::default(),
            args: vec![],
            idle_seconds: 0,
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
    f.state.reload_snapshot().await.unwrap();

    let out = crate::common::model_test_wire(
        modelinfo::local_model_test(&f.state, "chat", None)
            .await
            .unwrap(),
    );
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["probe"], "generate");
    let hits = f.server.received_requests().await.unwrap();
    assert!(hits.iter().any(|r| r.url.path() == "/v1/chat/completions"));

    // `container()` mounts no /props route, the shape every other test in
    // this file relies on — so this is also the "unreachable" case of the
    // running-build cross-check (design §8 item 9): the test still passes,
    // `props` is null rather than absent, and a note says why.
    assert_eq!(out["props"], Value::Null, "{out}");
    assert!(
        out["note"].as_str().is_some_and(|n| n.contains("/props")),
        "{out}"
    );
}

// ---------------------------------------------------------------------------
// The `/props` cross-check (model-capabilities design §8 item 9)
// ---------------------------------------------------------------------------

/// The running build's `/props` and the static derivation agree: `static` and
/// `props` are both reported, and `disagreements` is empty.
#[tokio::test]
async fn local_model_test_reports_props_and_an_empty_disagreement_list_when_they_agree() {
    let f = fixture(vec![0.1]).await;
    store::insert_local_model(
        &f.state.db,
        &NewLocalModel {
            model_id: "chat".into(),
            gguf_path: CHAT.into(),
            params: Default::default(),
            args: vec![],
            idle_seconds: 0,
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
    f.state.reload_snapshot().await.unwrap();

    // `CHAT`'s template (`synth::chat`) is `{{ messages }}` — no thinking
    // markers, no `tools` reference, no configured projector — so the static
    // derivation is tool_calls.kind = "none", reasoning.kind = "fixed",
    // input_modalities = ["text"]. A /props response that agrees with all of
    // that.
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chat_template_caps": {
                "supports_tools": false,
                "supports_parallel_tool_calls": false,
                "supports_reasoning_effort": false,
                "supports_preserve_reasoning": false
            },
            "modalities": { "vision": false, "audio": false, "video": false }
        })))
        .mount(&f.server)
        .await;

    let out = crate::common::model_test_wire(
        modelinfo::local_model_test(&f.state, "chat", None)
            .await
            .unwrap(),
    );
    assert_eq!(out["ok"], true, "{out}");
    assert!(!out["props"].is_null(), "{out}");
    assert!(!out["static"].is_null(), "{out}");
    assert_eq!(out["static"]["tool_calls"]["kind"], "none", "{out}");
    assert_eq!(out["static"]["reasoning"]["kind"], "fixed", "{out}");
    assert_eq!(out["disagreements"].as_array().unwrap().len(), 0, "{out}");
}

/// A row whose template renders tools (`tool_calls.kind == "native"`) but
/// whose running build's `/props` says otherwise: the mismatch is named,
/// and it names tools specifically.
#[tokio::test]
async fn local_model_test_names_a_tool_support_disagreement() {
    let f = fixture(vec![0.1]).await;
    let mut h = synth::Header::default();
    h.str("general.architecture", "qwen3")
        .str("general.name", "synthetic tool-calling chat model")
        .u32("qwen3.context_length", 32768)
        .u32("qwen3.block_count", 4)
        // Renders `tools` *and* a tool-call syntax the marker table knows, so
        // the static derivation says `native` — which is the claim this test
        // plays off against a /props that denies tool support. (A template
        // that renders tools in no recognised syntax is `text`, not `native`.)
        .str(
            "tokenizer.chat_template",
            "{% if tools %}<tool_call>{% endif %}{{ messages }}",
        );
    h.write_to(&f.chat_dir.path().join("tools/model.gguf"));

    store::insert_local_model(
        &f.state.db,
        &NewLocalModel {
            model_id: "tools-chat".into(),
            gguf_path: "tools/model.gguf".into(),
            params: Default::default(),
            args: vec![],
            idle_seconds: 0,
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
    f.state.reload_snapshot().await.unwrap();

    // The running build disagrees about tool support (and nothing else).
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chat_template_caps": {
                "supports_tools": false,
                "supports_parallel_tool_calls": false,
                "supports_reasoning_effort": false,
                "supports_preserve_reasoning": false
            },
            "modalities": { "vision": false, "audio": false, "video": false }
        })))
        .mount(&f.server)
        .await;

    let out = crate::common::model_test_wire(
        modelinfo::local_model_test(&f.state, "tools-chat", None)
            .await
            .unwrap(),
    );
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["static"]["tool_calls"]["kind"], "native", "{out}");
    let disagreements = out["disagreements"].as_array().unwrap();
    assert!(
        disagreements
            .iter()
            .any(|d| d.as_str().unwrap().contains("tools")),
        "expected a disagreement naming tools: {out}"
    );
}

// ---------------------------------------------------------------------------
// Static checks cover aux rows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_model_check_reports_aux_rows_that_contradict_their_header() {
    let f = fixture(vec![0.1]).await;
    // A reranker registered as an embedder, with mean pooling on top.
    store::insert_aux_model(
        &f.state.db,
        &aux_row("wrong", RERANKER, AuxKind::Embed, Some("mean")),
    )
    .await
    .unwrap();
    // A missing file.
    store::insert_aux_model(
        &f.state.db,
        &aux_row("gone", "nope/absent.gguf", AuxKind::Embed, None),
    )
    .await
    .unwrap();
    // A correct one.
    store::insert_aux_model(
        &f.state.db,
        &aux_row("fine", EMBEDDER, AuxKind::Embed, Some("last")),
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();

    let out =
        crate::common::check_wire(ops::local_model_check(&f.state, None, None).await.unwrap());
    assert_eq!(out["checked"], 3, "{out}");
    assert_eq!(out["broken"], 2, "{out}");
    let by_id = |id: &str| {
        out["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["model_id"] == id)
            .cloned()
    };
    let wrong = by_id("wrong").expect("reported");
    assert_eq!(wrong["class"], "aux");
    assert!(
        wrong["problems"].to_string().contains("classifier head"),
        "{wrong}"
    );
    let gone = by_id("gone").expect("reported");
    assert!(gone["problems"].to_string().contains("missing"), "{gone}");
    assert!(by_id("fine").is_none(), "a healthy row is omitted: {out}");

    // Naming one restricts to it, in either class.
    let one = crate::common::check_wire(
        ops::local_model_check(&f.state, Some("fine"), Some("aux"))
            .await
            .unwrap(),
    );
    assert_eq!(one["checked"], 1);
    assert_eq!(one["models"][0]["ok"], true, "{one}");
}

/// The flag vocabulary and a GGUF's header, read through the types the API
/// document describes; a field added to either answer without the type fails
/// here.
#[tokio::test]
async fn llama_flags_and_model_inspect_answer_their_documented_types() {
    let f = fixture(vec![0.1, 0.2]).await;

    let flags =
        serde_json::to_value(ops::llama_flags(&f.state, None, None).await.unwrap()).unwrap();
    let typed: lmgw_api_types::LlamaFlags = crate::common::round_trips("llama_flags", &flags);
    assert!(typed.flag_count > 0, "{flags}");
    assert_eq!(typed.flag_count, typed.flags.len());
    let narrowed =
        serde_json::to_value(ops::llama_flags(&f.state, Some("ctx"), None).await.unwrap()).unwrap();
    let narrowed: lmgw_api_types::LlamaFlags =
        crate::common::round_trips("llama_flags search", &narrowed);
    assert!(narrowed.flags.len() < narrowed.flag_count);

    let chat = crate::common::inspect_wire(
        lmgw_core::modelinfo::model_inspect(&f.state, CHAT, false, None)
            .await
            .unwrap(),
    );
    assert_eq!(chat["serve_as"], "chat", "{chat}");
    assert_eq!(
        chat["runtime"],
        json!({"checked": false, "reason": "probe not requested"})
    );
    let embed = crate::common::inspect_wire(
        lmgw_core::modelinfo::model_inspect(&f.state, EMBEDDER, false, Some("aux"))
            .await
            .unwrap(),
    );
    assert_eq!(embed["serve_as"], "aux", "{embed}");
    assert_eq!(embed["aux_kind"], "embed", "{embed}");
}
