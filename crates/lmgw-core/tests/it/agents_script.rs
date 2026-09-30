//! `script` steps end to end (container-runtime design §4.2, §4.3, §5.1, §9).
//!
//! Three halves, and the split is the same one WP2 made:
//!
//! - **No podman**: the warnings channel (`apply_turn`, `script_without_output`),
//!   `detail_inner`'s degradation, and the §5.1 built-in upgrade through the
//!   real `agent_reset` op.
//! - **Real podman** (`the_real_thing_*`): the shipped mail labeler's apply
//!   script, run by the real shim in the real Node image, against a **stub
//!   `gws` MCP server** that records every call. Nothing here touches a real
//!   mailbox: the gateway under test registers the stub as an ordinary HTTP MCP
//!   server with the `gws` prefix, so the script reaches it by exactly the name
//!   the shipped manifest uses. Skips itself with a message when podman is
//!   absent and removes every container it makes.
//!
//! The gateway is bound to a **real, saved** `bind_addr` here rather than left
//! at the default: `container::gateway_access` reads it to decide how a
//! container reaches back, and a run directory's `input.json` is the only other
//! thing the container is told.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::agents::{container, seed};
use lmgw_core::config::McpTransport;
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewMcpServer};
use serde_json::{json, Value};

use crate::common;
use common::{dashboard_key, Gw};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

static NEXT_PREFIX: AtomicU32 = AtomicU32::new(0);

/// A gateway whose `bind_addr` setting really is the port it is listening on.
///
/// The listener is bound *before* the settings are written, because a container
/// is told where the gateway is from `bind_addr` alone — a default of
/// `127.0.0.1:8001` would send the shim at nothing.
async fn gateway() -> (SharedState, Gw, String) {
    let state = AppState::init_for_tests().await.unwrap();
    let prefix = format!(
        "lmgws{}-{}",
        std::process::id(),
        NEXT_PREFIX.fetch_add(1, Ordering::Relaxed)
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.container_prefix = prefix.clone();
    settings.bind_addr = addr.to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let key = dashboard_key(&state);
    let app = build_router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        state,
        Gw {
            base: format!("http://{addr}"),
            key,
        },
        prefix,
    )
}

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

async fn get(base: &Gw, path: &str) -> (u16, Value) {
    let resp = base
        .client()
        .get(format!("{base}{path}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&body).unwrap_or(Value::String(body)),
    )
}

async fn detail(base: &Gw, id: &str) -> Value {
    let (status, v) = get(base, &format!("/api/agents/{id}")).await;
    assert_eq!(status, 200, "{v}");
    v
}

fn warning<'a>(d: &'a Value, code: &str) -> Option<&'a Value> {
    d["warnings"]
        .as_array()?
        .iter()
        .find(|w| w["code"] == json!(code))
}

async fn wait_done(base: &Gw, job_id: i64) -> Value {
    for _ in 0..1500 {
        let (_, d) = get(base, &format!("/api/agents/runs/{job_id}")).await;
        let status = d["job"]["status"].as_str().unwrap_or_default().to_string();
        if matches!(status.as_str(), "done" | "failed" | "canceled") {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("run {job_id} never finished");
}

async fn start(base: &Gw, args: Value) -> i64 {
    let (status, v) = op(base, "agent_run", args).await;
    assert_eq!(status, 200, "{v}");
    v["job_id"].as_i64().expect("a job id")
}

fn log(d: &Value) -> String {
    d["log"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The stub `gws` server
// ---------------------------------------------------------------------------

/// Every `tools/call` the script made, in order: `(tool name, arguments)`.
type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// A southbound MCP server that answers the five Gmail tools the shipped mail
/// labeler declares and records what it was asked to do.
///
/// `gmail_listLabels` says `lmgw/Work` already exists and `lmgw/Finance` does
/// not, so one run exercises both halves of the script: the label it finds and
/// the label it has to create.
async fn gws_stub(calls: Calls) -> String {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let made = Arc::new(Mutex::new(0u32));
    let handler = move |body: String| {
        let calls = calls.clone();
        let made = made.clone();
        async move {
            let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let Some(id) = req.get("id").cloned() else {
                return StatusCode::ACCEPTED.into_response();
            };
            let tool = |name: &str| {
                json!({
                    "name": name,
                    "description": name,
                    "inputSchema": { "type": "object", "properties": {} },
                })
            };
            let result = match req.get("method").and_then(Value::as_str).unwrap_or("") {
                "initialize" => json!({
                    "protocolVersion": req.pointer("/params/protocolVersion")
                        .cloned().unwrap_or(json!("2025-06-18")),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "gws-stub", "version": "0.1.0" },
                }),
                "tools/list" => json!({ "tools": [
                    tool("gmail_search"), tool("gmail_get"), tool("gmail_listLabels"),
                    tool("gmail_createLabel"), tool("gmail_batchModify"),
                ] }),
                "tools/call" => {
                    let name = req
                        .pointer("/params/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let args = req
                        .pointer("/params/arguments")
                        .cloned()
                        .unwrap_or(json!({}));
                    calls.lock().unwrap().push((name.clone(), args.clone()));
                    let structured = match name.as_str() {
                        "gmail_listLabels" => json!({ "labels": [
                            { "id": "INBOX", "name": "INBOX" },
                            { "id": "UNREAD", "name": "UNREAD" },
                            { "id": "Label_7", "name": "lmgw/Work" },
                        ] }),
                        "gmail_createLabel" => {
                            let mut n = made.lock().unwrap();
                            *n += 1;
                            json!({ "id": format!("Label_new_{n}"),
                                    "name": args.get("name").cloned().unwrap_or(Value::Null) })
                        }
                        _ => json!({ "ok": true }),
                    };
                    json!({
                        // Both halves, so the shim's "structuredContent, else
                        // the first text block parsed as JSON" is exercised the
                        // way a real server would exercise it.
                        "content": [{ "type": "text", "text": structured.to_string() }],
                        "structuredContent": structured,
                        "isError": false,
                    })
                }
                _ => json!({}),
            };
            (
                StatusCode::OK,
                [
                    ("content-type", "application/json"),
                    ("mcp-session-id", "gws-stub-session"),
                ],
                json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
            )
                .into_response()
        }
    };

    let app = axum::Router::new().route("/mcp", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

async fn register_gws(state: &SharedState, url: &str) {
    store::insert_mcp_server(
        &state.db,
        &NewMcpServer {
            name: "gws-stub".into(),
            enabled: true,
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: vec![],
            cwd: None,
            container_image: None,
            extra_run_args: vec![],
            url: Some(url.into()),
            headers: vec![],
            tool_prefix: "gws".into(),
            timeout_ms: 10_000,
            autostart: false,
            idle_seconds: 300,
            allow_sampling: false,
            sampling_alias: None,
            agent_id: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    state.mcp.reconcile(&state.snapshot()).await;
}

// ---------------------------------------------------------------------------
// §4.3 — the warnings channel and the degraded detail (no podman)
// ---------------------------------------------------------------------------

/// A manifest with `apply.turn` — the shape every install before this version
/// stored. It **imports, loads and lists**; Start is disabled with the reason.
fn turn_doc() -> String {
    json!({
        "schema_version": 1,
        "id": "old-tagger",
        "name": "Old tagger",
        "model": { "alias": "{{config.model}}" },
        "config": { "schema": { "type": "object", "properties": {
            "model": { "type": "string", "format": "model_alias", "default": "m1" }
        } } },
        "run": { "kind": "batch",
            "source": { "tool": "gws__gmail_search" },
            "item": { "id": "{{item.id}}" },
            "apply": { "turn": { "prompt": "write it" } } }
    })
    .to_string()
}

#[tokio::test]
async fn an_apply_turn_loads_and_warns_and_its_detail_is_not_a_400() {
    let (_state, base, _prefix) = gateway().await;
    let (status, report) = op(
        &base,
        "agent_set",
        json!({ "manifest": turn_doc(), "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "{report}");

    // The card lists, carrying the reason.
    let (status, cards) = get(&base, "/api/agents").await;
    assert_eq!(status, 200, "{cards}");
    let card = cards
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == json!("old-tagger"))
        .expect("the agent was saved despite the apply turn");
    assert!(card["error"].is_null(), "{card}");

    // The detail is a document, not a 400 — the Definition editor has to be
    // reachable for the very manifest that needs editing (§4.3).
    let d = detail(&base, "old-tagger").await;
    let w = warning(&d, "apply_turn").unwrap_or_else(|| panic!("no apply_turn warning: {d}"));
    assert_eq!(w["blocks_start"], json!(true), "{d}");
    assert!(
        w["message"]
            .as_str()
            .unwrap()
            .contains("apply may not run a model turn"),
        "{d}"
    );

    // And Start really is refused, naming the same thing.
    let (status, res) = op(
        &base,
        "agent_run",
        json!({ "id": "old-tagger", "phase": "list" }),
    )
    .await;
    assert_ne!(status, 200, "{res}");
    let msg = res.to_string();
    assert!(msg.contains("apply_turn"), "{msg}");
}

/// A row this build cannot read at all: the detail comes back `200` with
/// `error` set and the stored text intact, rather than the `400` that used to
/// lock the owner out of the editor (§4.3).
#[tokio::test]
async fn an_unreadable_manifest_degrades_instead_of_answering_400() {
    let (state, base, _prefix) = gateway().await;
    store::insert_agent(
        &state.db,
        "from-the-future",
        // Readable JSON, unreadable manifest: a `run.kind` this build has never
        // heard of, exactly what a downgrade produces.
        &json!({
            "schema_version": 1, "id": "from-the-future", "name": "Tomorrow",
            "description": "written by a newer build",
            "model": { "alias": "m" },
            "run": { "kind": "swarm" }
        })
        .to_string(),
        "imported",
    )
    .await
    .unwrap();

    let (status, d) = get(&base, "/api/agents/from-the-future").await;
    assert_eq!(status, 200, "a degraded row must not 400: {d}");
    assert!(
        d["error"].as_str().unwrap_or_default().contains("swarm"),
        "{d}"
    );
    // What could still be read is there, and the raw text is verbatim so the
    // Definition tab can open it.
    assert_eq!(d["name"], json!("Tomorrow"));
    assert_eq!(d["source"], json!("imported"));
    assert!(d["manifest"].as_str().unwrap().contains("\"swarm\""), "{d}");
    assert_eq!(d["requires_ok"], json!(false));
    assert_eq!(
        warning(&d, "manifest_unreadable").map(|w| w["blocks_start"].clone()),
        Some(json!(true)),
        "{d}"
    );

    // A genuinely missing agent is still a 404.
    let (status, _) = get(&base, "/api/agents/nobody").await;
    assert_eq!(status, 404);
}

// ---------------------------------------------------------------------------
// §5.1 — the built-in upgrade through the real op
// ---------------------------------------------------------------------------

/// The 2.0.0 shape: the shipped document with the retired `apply.turn`.
fn mail_2_0_0() -> String {
    let m = seed::shipped("mail-labeler").expect("the mail agent ships");
    let mut v: Value = serde_json::from_str(&m.to_json()).unwrap();
    v["version"] = json!("2.0.0");
    v["run"]["apply"] = json!({ "turn": {
        "tools": ["gws__gmail_batchModify"],
        "prompt": "Label every row: {{rows}}",
        "output": { "type": "object",
                    "properties": { "applied": { "type": "integer" },
                                    "labels": { "type": "array", "items": { "type": "string" } } },
                    "required": ["applied", "labels"] } } });
    v.to_string()
}

/// An older install: a `2.0.0` row seeded before hashes were recorded. The
/// card shows the notice, and **one Reset to shipped** adopts `3.0.0`, keeps
/// the taxonomy and clears both warnings.
#[tokio::test]
async fn the_newer_manifest_notice_and_one_reset_adopts_it_keeping_the_config() {
    let (state, base, _prefix) = gateway().await;
    store::update_agent_manifest(&state.db, "mail-labeler", &mail_2_0_0())
        .await
        .unwrap();
    store::set_agent_config(
        &state.db,
        "mail-labeler",
        &json!({ "model": "m1", "categories": ["Work", "Bills"], "label_prefix": "mail" })
            .to_string(),
    )
    .await
    .unwrap();
    // The legacy KV form, which is what an install from before this version
    // has: seeded, hash unknown.
    store::set_kv(
        &state.db,
        lmgw_core::agents::SEEDED_KEY,
        &json!(["mail-labeler", "docs-librarian"]).to_string(),
    )
    .await
    .unwrap();
    seed::seed(&state).await;

    let d = detail(&base, "mail-labeler").await;
    assert_eq!(d["version"], json!("2.0.0"), "the row was replaced anyway");
    assert_eq!(d["resettable"], json!(true));
    let notice = warning(&d, "builtin_update_available")
        .unwrap_or_else(|| panic!("no builtin_update_available notice: {d}"));
    assert_eq!(notice["blocks_start"], json!(false), "a notice, not a gate");
    assert!(
        notice["message"]
            .as_str()
            .unwrap()
            .contains("newer built-in manifest"),
        "{d}"
    );
    // And the retired apply turn is what is blocking Start until it is adopted.
    assert_eq!(
        warning(&d, "apply_turn").map(|w| w["blocks_start"].clone()),
        Some(json!(true)),
        "{d}"
    );

    let (status, res) = op(&base, "agent_reset", json!({ "id": "mail-labeler" })).await;
    assert_eq!(status, 200, "{res}");

    let d = detail(&base, "mail-labeler").await;
    assert_eq!(d["version"], json!("3.0.0"));
    assert!(warning(&d, "builtin_update_available").is_none(), "{d}");
    assert!(warning(&d, "apply_turn").is_none(), "{d}");
    // The taxonomy survived the reset, which is the whole point of it keeping
    // the config.
    assert_eq!(d["config"]["categories"], json!(["Work", "Bills"]));
    assert_eq!(d["config"]["label_prefix"], json!("mail"));

    // The hash went in with the row, so a restart reads the row as current
    // rather than as edited and pinned.
    seed::seed(&state).await;
    let d = detail(&base, "mail-labeler").await;
    assert_eq!(d["version"], json!("3.0.0"));
    assert!(warning(&d, "builtin_update_available").is_none(), "{d}");
}

/// A box with no podman cannot run a script step — but it can still list and
/// classify, so the notice says that instead of disabling Start outright
/// (§4.3). Every test gateway runs with the `NoSpawner`, which is exactly that
/// box.
#[tokio::test]
async fn a_scripted_agent_without_podman_warns_without_taking_away_the_rest() {
    let (_state, base, _prefix) = gateway().await;
    let d = detail(&base, "mail-labeler").await;
    let w = warning(&d, "podman_unavailable")
        .unwrap_or_else(|| panic!("no podman_unavailable warning: {d}"));
    assert_eq!(
        w["blocks_start"],
        json!(false),
        "a missing podman must not disable the classify half: {d}"
    );
    assert!(
        w["message"].as_str().unwrap().contains("Apply will fail"),
        "{d}"
    );
    // The shipped mail labeler declares no `output`-less script, so nothing
    // else is flagged about its apply step.
    assert!(warning(&d, "script_without_output").is_none(), "{d}");
    assert!(warning(&d, "apply_turn").is_none(), "{d}");
}

/// A script step runs under the manifest's `run.limits` in a container, so the
/// Runtime block prints every one of them — with the script image and `missing`
/// in place of the image and pull policy a batch manifest does not carry
/// (§4.2). A bound the owner is under is a bound the owner can see.
#[tokio::test]
async fn a_scripted_agent_prints_the_limits_its_script_step_runs_under() {
    let (_state, base, _prefix) = gateway().await;
    let d = detail(&base, "mail-labeler").await;
    let r = &d["runtime"];
    assert!(!r.is_null(), "no Runtime block for a scripted agent: {d}");
    assert_eq!(r["image"], json!("docker.io/library/node:24-alpine"));
    assert_eq!(r["pull"], json!("missing"));
    // The WP2 defaults, printed rather than applied behind the owner's back.
    assert_eq!(r["memory_mb"], json!(512));
    assert_eq!(r["cpus"], json!(2.0));
    assert_eq!(r["pids"], json!(256));
    assert_eq!(r["deadline_seconds"], json!(600));
    assert_eq!(r["stop_grace_seconds"], json!(10));
    assert_eq!(r["read_only"], json!(true));

    // A `chat` agent has no run of its own, so there is nothing to print.
    let d = detail(&base, "docs-librarian").await;
    assert!(d["runtime"].is_null(), "{d}");
}

/// The script image is a visible setting with a printed default, not a constant.
#[tokio::test]
async fn the_script_image_is_a_settings_field_that_round_trips() {
    let (_state, base, _prefix) = gateway().await;
    let (status, s) = get(&base, "/api/settings-full").await;
    assert_eq!(status, 200, "{s}");
    assert_eq!(
        s["agent_script_image"],
        json!("docker.io/library/node:24-alpine")
    );

    let (status, res) = op(
        &base,
        "settings_set_full",
        json!({ "agent_script_image": "localhost/my-node:1" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let (_, s) = get(&base, "/api/settings-full").await;
    assert_eq!(s["agent_script_image"], json!("localhost/my-node:1"));

    // It is the image operand of a `podman run`, so a value that would become a
    // flag or several words is refused where the owner is looking rather than
    // at the first Start of a script step.
    for bad in ["--privileged", "node:24-alpine --rm"] {
        let (status, res) = op(
            &base,
            "settings_set_full",
            json!({ "agent_script_image": bad }),
        )
        .await;
        assert_ne!(status, 200, "{bad} was accepted: {res}");
    }
    let (_, s) = get(&base, "/api/settings-full").await;
    assert_eq!(
        s["agent_script_image"],
        json!("localhost/my-node:1"),
        "a refused patch changed the stored value"
    );

    // Blanking it restores the shipped default rather than leaving a script
    // step with no image to run in, and says so.
    let (status, res) = op(
        &base,
        "settings_set_full",
        json!({ "agent_script_image": "  " }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let (_, s) = get(&base, "/api/settings-full").await;
    assert_eq!(
        s["agent_script_image"],
        json!("docker.io/library/node:24-alpine")
    );
}

// ---------------------------------------------------------------------------
// The real thing (needs podman)
// ---------------------------------------------------------------------------

fn podman_available() -> bool {
    std::process::Command::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Every container the run may have left, removed by name whatever happened.
struct ContainerSweep(String);

impl Drop for ContainerSweep {
    fn drop(&mut self) {
        if let Ok(out) = std::process::Command::new("podman")
            .args([
                "ps",
                "-aq",
                "--filter",
                &format!("label=lmgw.instance={}", self.0),
            ])
            .output()
        {
            for id in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                let _ = std::process::Command::new("podman")
                    .args(["rm", "-f", id])
                    .output();
            }
        }
    }
}

/// Set the mail labeler up for an apply: a model in the config (it is required,
/// and the point is that apply never uses it) and the prefix §9's script reads.
async fn configure_mail(state: &SharedState) {
    store::set_agent_config(
        &state.db,
        "mail-labeler",
        &json!({ "model": "m1", "label_prefix": "lmgw" }).to_string(),
    )
    .await
    .unwrap();
}

fn called(calls: &Calls, name: &str) -> Vec<Value> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, a)| a.clone())
        .collect()
}

/// §9, proven: the shipped apply script, the real shim, the real Node image,
/// and a stub `gws` that records what it was told to write.
#[tokio::test]
async fn the_real_thing_labels_every_row_with_one_batch_modify_per_label() {
    if !podman_available() {
        eprintln!("SKIP the_real_thing_labels…: podman is not available on this box");
        return;
    }
    let (state, base, prefix) = gateway().await;
    let _sweep = ContainerSweep(prefix.clone());
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let url = gws_stub(calls.clone()).await;
    register_gws(&state, &url).await;
    configure_mail(&state).await;
    state.set_agent_spawner_for_tests(Arc::new(container::TokioSpawner));

    // The gap the card would otherwise show: every tool the manifest names has
    // to resolve, or Start is disabled before podman is ever reached.
    let d = detail(&base, "mail-labeler").await;
    assert_eq!(d["requires_ok"], json!(true), "{d}");
    assert_eq!(d["batch"]["apply_tools_are_ceiling"], json!(true), "{d}");

    let rows = json!([
        { "id": "m1", "output": { "category": "Work" } },
        { "id": "m2", "output": { "category": "Work" } },
        { "id": "m3", "output": { "category": "Finance" } },
    ]);
    let job = start(
        &base,
        json!({ "id": "mail-labeler", "phase": "apply", "rows": rows }),
    )
    .await;
    let d = wait_done(&base, job).await;
    assert_eq!(d["job"]["status"], json!("done"), "{}", log(&d));

    // `{applied, labels}` — the contract the retired turn carried, validated
    // against the step's own `output` schema before it was stored.
    assert_eq!(
        d["result"]["applied"]["output"],
        json!({ "applied": 3, "labels": ["lmgw/Work", "lmgw/Finance"] }),
        "{}",
        log(&d)
    );

    // One listLabels, one createLabel for the label that did not exist, and
    // **exactly one batchModify per distinct label**.
    assert_eq!(called(&calls, "gmail_listLabels").len(), 1);
    let created = called(&calls, "gmail_createLabel");
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(created[0]["name"], json!("lmgw/Finance"));

    let modified = called(&calls, "gmail_batchModify");
    assert_eq!(modified.len(), 2, "one call per label: {modified:?}");
    let work = &modified[0];
    assert_eq!(work["messageIds"], json!(["m1", "m2"]));
    assert_eq!(work["addLabelIds"], json!(["Label_7"]), "the existing id");
    let finance = &modified[1];
    assert_eq!(finance["messageIds"], json!(["m3"]));
    assert_eq!(
        finance["addLabelIds"],
        json!(["Label_new_1"]),
        "the id createLabel returned"
    );

    // The guarantee moved from a sentence in a system prompt to the absence of
    // a key: `removeLabelIds` is never constructed, so `UNREAD` cannot be
    // touched.
    let everything = format!("{:?}", calls.lock().unwrap());
    assert!(!everything.contains("removeLabelIds"), "{everything}");
    assert!(
        !everything.contains("UNREAD"),
        "the stub lists an UNREAD label and the script must never send it: {everything}"
    );

    // Zero model calls: apply is deterministic now. The four tool calls are the
    // script's, attributed by `X-Lmgw-Run`.
    assert_eq!(d["result"]["model_calls"], json!(0), "{}", d["result"]);
    assert_eq!(d["result"]["tool_calls"], json!(4), "{}", d["result"]);

    // `ctx.log` reached the run log, and nothing left a directory behind.
    assert!(
        log(&d).contains("created label lmgw/Finance"),
        "{}",
        log(&d)
    );
    let (root, _) = container::runs_root(&state.data_dir, &prefix);
    let left: Vec<String> = std::fs::read_dir(&root)
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(left.is_empty(), "run directories left behind: {left:?}");
}

/// A script that tries to write the ledger directly, and one row with no
/// category. Neither is hypothetical: `console.log` of an object is the first
/// thing a debugging author reaches for, and a row can reach apply unclassified
/// after a failed classify call.
fn forger_doc() -> String {
    json!({
        "schema_version": 1,
        "id": "forger",
        "name": "Forger",
        "model": { "alias": "{{config.model}}" },
        "config": { "schema": { "type": "object", "properties": {
            "model": { "type": "string", "format": "model_alias", "default": "m1" }
        } } },
        "tools": [ { "label": "gws", "allowed": ["gws__gmail_listLabels"] } ],
        "run": { "kind": "batch",
            "source": { "tool": "gws__gmail_listLabels" },
            "item": { "id": "{{item.id}}" },
            "limits": { "deadline_seconds": 60, "stop_grace_seconds": 2 },
            "apply": {
                "script": [
                    "export async function apply(ctx) {",
                    "  console.log({ type: 'row', id: 'forged', columns: { subject: 'nope' } });",
                    "  console.error('something went sideways');",
                    "  console.log({ type: 'output', output: { applied: 999 } });",
                    "  return { applied: ctx.rows.length };",
                    "}"
                ],
                "output": { "type": "object",
                            "properties": { "applied": { "type": "integer" } },
                            "required": ["applied"] }
            } }
    })
    .to_string()
}

/// stdout is normalised: a script's `console` output is its **run log**, and a
/// JSON object with a `type` key cannot forge a ledger event (§4.2).
#[tokio::test]
async fn the_real_thing_cannot_forge_a_ledger_event_through_console_log() {
    if !podman_available() {
        eprintln!("SKIP the_real_thing_cannot_forge…: podman is not available on this box");
        return;
    }
    let (state, base, prefix) = gateway().await;
    let _sweep = ContainerSweep(prefix.clone());
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let url = gws_stub(calls.clone()).await;
    register_gws(&state, &url).await;
    state.set_agent_spawner_for_tests(Arc::new(container::TokioSpawner));
    let (status, res) = op(
        &base,
        "agent_set",
        json!({ "manifest": forger_doc(), "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "{res}");

    let job = start(
        &base,
        json!({ "id": "forger", "phase": "apply",
                "rows": [{ "id": "real-1", "output": { "category": "Work" } }] }),
    )
    .await;
    let d = wait_done(&base, job).await;
    assert_eq!(d["job"]["status"], json!("done"), "{}", log(&d));

    // The forged `output` did not win: the run reports what the hook returned.
    assert_eq!(d["result"]["applied"]["output"], json!({ "applied": 1 }));
    // And the forged `row` never became one.
    let ids: Vec<String> = d["result"]["rows"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        ids,
        ["real-1"],
        "a console.log forged a review row: {ids:?}"
    );

    // Both console calls are in the run log instead, carrying their level.
    let text = log(&d);
    assert!(text.contains("something went sideways"), "{text}");
    assert!(
        text.contains("nope"),
        "the forged line vanished entirely: {text}"
    );
    assert!(text.contains("[error]") || text.contains("error"), "{text}");
}

/// A row that reaches apply with no category is skipped and said out loud,
/// rather than labelled `<prefix>/undefined` (§9).
#[tokio::test]
async fn the_real_thing_skips_a_row_that_has_no_category() {
    if !podman_available() {
        eprintln!("SKIP the_real_thing_skips…: podman is not available on this box");
        return;
    }
    let (state, base, prefix) = gateway().await;
    let _sweep = ContainerSweep(prefix.clone());
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let url = gws_stub(calls.clone()).await;
    register_gws(&state, &url).await;
    configure_mail(&state).await;
    state.set_agent_spawner_for_tests(Arc::new(container::TokioSpawner));

    let job = start(
        &base,
        json!({ "id": "mail-labeler", "phase": "apply", "rows": [
            { "id": "m1", "output": { "category": "Work" } },
            { "id": "m2", "output": {} },
        ] }),
    )
    .await;
    let d = wait_done(&base, job).await;
    assert_eq!(d["job"]["status"], json!("done"), "{}", log(&d));

    // One label, one message, and the skipped row said so.
    assert_eq!(
        d["result"]["applied"]["output"],
        json!({ "applied": 1, "labels": ["lmgw/Work"] }),
        "{}",
        log(&d)
    );
    assert!(log(&d).contains("skipping m2"), "{}", log(&d));
    let everything = format!("{:?}", calls.lock().unwrap());
    assert!(!everything.contains("undefined"), "{everything}");
    let modified = called(&calls, "gmail_batchModify");
    assert_eq!(modified.len(), 1, "{modified:?}");
    assert_eq!(modified[0]["messageIds"], json!(["m1"]));
}

/// A batch agent whose apply step loops: one call, a pause, the next. The
/// SIGTERM handler is what turns Cancel into "the call in flight finishes and
/// every later one is refused" rather than a SIGKILL mid-write (§4.2).
fn looping_doc() -> String {
    json!({
        "schema_version": 1,
        "id": "looper",
        "name": "Looper",
        "model": { "alias": "{{config.model}}" },
        "config": { "schema": { "type": "object", "properties": {
            "model": { "type": "string", "format": "model_alias", "default": "m1" }
        } } },
        "tools": [ { "label": "gws", "allowed": ["gws__gmail_listLabels"] } ],
        "run": { "kind": "batch",
            "source": { "tool": "gws__gmail_listLabels" },
            "item": { "id": "{{item.id}}" },
            "limits": { "deadline_seconds": 120, "stop_grace_seconds": 5 },
            "apply": {
                "script": [
                    "const sleep = (ms) => new Promise(r => setTimeout(r, ms));",
                    "export async function apply(ctx) {",
                    "  let n = 0;",
                    "  for (let i = 0; i < 400; i++) {",
                    "    await ctx.tools.call('gws__gmail_listLabels', {});",
                    "    n += 1;",
                    "    ctx.log(`call ${n}`);",
                    "    await sleep(1500);",
                    "  }",
                    "  return { applied: n };",
                    "}"
                ],
                "output": { "type": "object",
                            "properties": { "applied": { "type": "integer" } },
                            "required": ["applied"] }
            } }
    })
    .to_string()
}

#[tokio::test]
async fn the_real_thing_stops_calling_tools_the_moment_cancel_lands() {
    if !podman_available() {
        eprintln!("SKIP the_real_thing_stops_calling…: podman is not available on this box");
        return;
    }
    let (state, base, prefix) = gateway().await;
    let _sweep = ContainerSweep(prefix.clone());
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let url = gws_stub(calls.clone()).await;
    register_gws(&state, &url).await;
    state.set_agent_spawner_for_tests(Arc::new(container::TokioSpawner));
    let (status, res) = op(
        &base,
        "agent_set",
        json!({ "manifest": looping_doc(), "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "{res}");

    let job = start(
        &base,
        json!({ "id": "looper", "phase": "apply",
                "rows": [{ "id": "x", "output": { "category": "Work" } }] }),
    )
    .await;
    // Wait until the script has actually started calling, so the cancel lands
    // inside the loop rather than before the container came up.
    for _ in 0..1500 {
        if !calls.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let before = calls.lock().unwrap().len();
    assert!(before > 0, "the script never got going: {before}");
    // Land the cancel while the script is *sleeping* rather than calling: that
    // is the case the handler's own exit exists for, and the one that used to
    // wait out the whole grace.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let pressed = std::time::Instant::now();
    let (status, res) = op(&base, "agent_run_cancel", json!({ "id": "looper" })).await;
    assert_eq!(status, 200, "{res}");
    let d = wait_done(&base, job).await;
    let elapsed = pressed.elapsed();
    assert_eq!(d["job"]["status"], json!("canceled"), "{}", log(&d));

    // The container went **on its own**, not by SIGKILL. The shim's handler
    // exits as soon as the last in-flight call lands, so the run log carries
    // "exited with status 1" — not signal 9 (exit 137), which is what podman's
    // SIGKILL at the end of `stop_grace_seconds` would have left, and is what
    // this actually did before the handler learned to leave.
    let text = log(&d);
    assert!(
        !text.contains("status 137") && !text.contains("killed by signal"),
        "the script had to be SIGKILLed instead of leaving on its own:\n{text}"
    );
    // Well inside the grace: the manifest asks for 5 s and the ladder logs each
    // rung, so a stop that had to escalate would say so.
    assert!(
        !text.contains("still there after stop"),
        "the stop ladder had to escalate:\n{text}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "cancel took {elapsed:?}, which is the SIGKILL path, not the graceful one"
    );

    let after = calls.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        calls.lock().unwrap().len(),
        after,
        "tool calls kept arriving after the run ended"
    );
    assert!(
        after < 400,
        "the loop ran to completion instead of being cancelled"
    );
}
