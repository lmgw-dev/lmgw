//! `run.kind = "container"` end to end (container-runtime design §6, WP2).
//!
//! Two halves, and the split is the point:
//!
//! - **Against a fake spawner** (the bulk): the whole executor path runs for
//!   real — the `agent_run` op, the job row, the `(kind, key)` guard, the JSONL
//!   decoder, the review table, cancel, the deadline and the job `result` —
//!   with nothing started on the box. The fake replays scripted stdout/stderr
//!   and an exit code, and *reads the files lmgw wrote for the mount*, so the
//!   `input.json` and `secrets.json` contracts are asserted where they are
//!   actually produced rather than in a unit test of the producer.
//! - **Against real podman** (`the_real_thing_*`): one hand-built image over
//!   `registry.fedoraproject.org/fedora-minimal:44` that echoes JSONL, run
//!   through the same path. Skips itself with a message when podman is absent,
//!   and removes its image and every container it makes, including on failure.
//!
//! Every gateway here gets its own `container_prefix`, which is what keeps two
//! tests in the same process off each other's container names and run
//! directories — the same reason production uses it to keep a dev instance off
//! the real one.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use lmgw_core::agents::container::{self, Exit, Spawned, Spawner};
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::common;
use common::{serve, Gw};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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

async fn get_json(base: &Gw, path: &str) -> Value {
    let resp = base
        .client()
        .get(format!("{base}{path}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "GET {path}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("GET {path} is not JSON ({e}): {body}"))
}

/// A distinct `container_prefix` per gateway. Two tests in one process share
/// `$XDG_RUNTIME_DIR` and both start their job ids at 1, so without this they
/// would fight over `run-1` — exactly the collision the prefix exists to
/// remove.
static NEXT_PREFIX: AtomicU32 = AtomicU32::new(0);

async fn gateway() -> (SharedState, String) {
    let state = AppState::init_for_tests().await.unwrap();
    let prefix = format!(
        "lmgwt{}-{}",
        std::process::id(),
        NEXT_PREFIX.fetch_add(1, Ordering::Relaxed)
    );
    let mut settings = state.snapshot().settings.clone();
    settings.container_prefix = prefix.clone();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    (state, prefix)
}

/// A container agent: two review columns, both phases, an output schema on the
/// apply side, and one secret config field so `secrets.json` has something to
/// carry besides the token.
fn doc(image: &str, deadline_seconds: u64, phases: &str, output: &str) -> String {
    // The review gate only makes sense in front of a phase the image
    // implements, and the manifest refuses it otherwise.
    let review = if phases.contains("apply") {
        r#""review": { "editable": ["category"] },"#
    } else {
        ""
    };
    format!(
        r#"{{
  "schema_version": 1,
  "id": "labeler",
  "name": "Labeler",
  "model": {{ "alias": "{{{{config.model}}}}" }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "model": {{ "type": "string", "format": "model_alias" }},
    "label_prefix": {{ "type": "string", "default": "ai" }},
    "api_token": {{ "type": "string", "format": "secret" }}
  }} }} }},
  "run": {{
    "kind": "container",
    "image": "{image}",
    "columns": ["subject"],
    {review}
    "phases": {phases},
    "limits": {{ "memory_mb": 64, "cpus": 1.0, "pids": 32,
                "deadline_seconds": {deadline_seconds}, "stop_grace_seconds": 1 }}
    {output}
  }}
}}"#
    )
}

/// `run.output` is a **per-phase** map, so the fixture declares the same
/// `{applied}` schema for every phase the image implements.
fn applied_schema(phases: &str) -> String {
    let per: Vec<String> = ["run", "apply"]
        .iter()
        .filter(|p| phases.contains(*p))
        .map(|p| {
            format!(
                r#""{p}": {{ "type": "object",
                  "properties": {{ "applied": {{ "type": "integer" }} }},
                  "required": ["applied"] }}"#
            )
        })
        .collect();
    format!(r#", "output": {{ {} }}"#, per.join(", "))
}

async fn install(base: &Gw, manifest: &str) {
    let (status, body) = op(
        base,
        "agent_set",
        json!({ "manifest": manifest, "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "agent_set: {body}");
}

/// Poll a run until it is no longer queued or running.
async fn wait_done(base: &Gw, job_id: i64) -> Value {
    for _ in 0..600 {
        let d = get_json(base, &format!("/api/agents/runs/{job_id}")).await;
        let status = d["job"]["status"].as_str().unwrap_or_default().to_string();
        if status != "queued" && status != "running" {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("run {job_id} never finished");
}

async fn start(base: &Gw, args: Value) -> i64 {
    let (status, res) = op(base, "agent_run", args).await;
    assert_eq!(status, 200, "agent_run: {res}");
    res["job_id"].as_i64().unwrap()
}

fn rows(d: &Value) -> Vec<Value> {
    d["rows"].as_array().cloned().unwrap_or_default()
}

/// The codes of the warnings that disable Start (§4.3).
fn blocking_codes(detail: &Value) -> Vec<String> {
    detail["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|w| w["blocks_start"] == json!(true))
        .filter_map(|w| w["code"].as_str().map(str::to_string))
        .collect()
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
// The fake spawner
// ---------------------------------------------------------------------------

/// One recorded `podman` invocation, with whatever lmgw had written for its
/// bind mounts read back off disk.
#[derive(Debug, Clone)]
struct Call {
    argv: Vec<String>,
    /// `(/lmgw/<name>, file contents)` for every `-v` this invocation carried.
    mounted: Vec<(String, String)>,
}

impl Call {
    fn env(&self, name: &str) -> Option<String> {
        let want = format!("{name}=");
        self.argv
            .iter()
            .find(|a| a.starts_with(&want))
            .map(|a| a[want.len()..].to_string())
    }
    fn mount(&self, inside: &str) -> Value {
        let (_, body) = self
            .mounted
            .iter()
            .find(|(k, _)| k == inside)
            .unwrap_or_else(|| panic!("no mount at {inside}: {:?}", self.mounted));
        serde_json::from_str(body).unwrap_or_else(|e| panic!("{inside} is not JSON ({e}): {body}"))
    }
}

/// Replays scripted stdout/stderr and an exit code, the same arrangement
/// `tests/it/runtime_registry.rs`'s fake `CommandRunner` uses for podman.
#[derive(Default)]
struct FakeSpawner {
    stdout: Vec<String>,
    stderr: Vec<String>,
    exit: Option<Exit>,
    /// Hold the pipes open until `podman stop` arrives — which is how a cancel
    /// and a deadline are exercised without a real container.
    hold: bool,
    /// Close the pipes at once but **never exit**, and refuse every `stop`:
    /// the wedged container the stop ladder exists for.
    wedged: bool,
    calls: Arc<Mutex<Vec<Call>>>,
    stopped: Arc<tokio::sync::Notify>,
}

impl FakeSpawner {
    fn new(stdout: &[&str]) -> Self {
        Self {
            stdout: stdout.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }
    fn stderr(mut self, lines: &[&str]) -> Self {
        self.stderr = lines.iter().map(|s| (*s).to_string()).collect();
        self
    }
    fn exit(mut self, code: i32) -> Self {
        self.exit = Some(Exit::Code(code));
        self
    }
    fn holds(mut self) -> Self {
        self.hold = true;
        self
    }
    /// Pipes close, the process never ends, and `podman stop` fails — so only
    /// the ladder's last rung (dropping the child) can end the run.
    fn wedged(mut self) -> Self {
        self.wedged = true;
        self
    }
    fn calls(&self) -> Arc<Mutex<Vec<Call>>> {
        self.calls.clone()
    }
}

/// Read every `-v host:inside:ro,Z` back off disk, while the run directory
/// still exists.
fn mounted_files(argv: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (i, a) in argv.iter().enumerate() {
        if a != "-v" {
            continue;
        }
        let Some(spec) = argv.get(i + 1) else {
            continue;
        };
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() < 2 {
            continue;
        }
        let body = std::fs::read_to_string(parts[0]).unwrap_or_default();
        out.push((parts[1].to_string(), body));
    }
    out
}

/// A spawner whose `podman image exists` says "no". Everything else answers
/// like the real thing does when it is happy.
struct NoImage;

#[async_trait]
impl Spawner for NoImage {
    async fn spawn(&self, _p: &str, _a: &[String]) -> std::io::Result<Spawned> {
        panic!("Start must be refused before anything is spawned");
    }
    async fn run(&self, _p: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        // `podman image exists` answers with its exit code alone: 1 is "no".
        let status = i32::from(args.first().map(String::as_str) == Some("image"));
        Ok(CmdOutput {
            status,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

#[tokio::test]
async fn an_absent_image_with_pull_never_blocks_start_instead_of_downloading() {
    let (state, _prefix) = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(NoImage));
    let base = serve(state.clone()).await;
    install(&base, &doc("localhost/labeler:1", 600, r#"["run"]"#, "")).await;

    let detail = get_json(&base, "/api/agents/labeler").await;
    assert_eq!(
        blocking_codes(&detail),
        ["image_absent_pull_never"],
        "{detail}"
    );
    assert_eq!(detail["runtime"]["podman"], true, "{detail}");

    // Refused before anything is started: the fake panics if it ever is.
    let (status, res) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "run" }),
    )
    .await;
    assert_ne!(status, 200, "{res}");
    let msg = res.to_string();
    assert!(msg.contains("image_absent_pull_never"), "{msg}");
    assert!(msg.contains("run.pull is 'never'"), "{msg}");
}

#[async_trait]
impl Spawner for FakeSpawner {
    async fn spawn(&self, program: &str, args: &[String]) -> std::io::Result<Spawned> {
        assert_eq!(program, "podman");
        self.calls.lock().unwrap().push(Call {
            argv: args.to_vec(),
            mounted: mounted_files(args),
        });
        let (out_tx, stdout) = mpsc::channel(64);
        let (err_tx, stderr) = mpsc::channel(64);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let (lines, errs) = (self.stdout.clone(), self.stderr.clone());
        let (hold, wedged, stopped, exit) = (
            self.hold,
            self.wedged,
            self.stopped.clone(),
            self.exit.unwrap_or(Exit::Code(0)),
        );
        tokio::spawn(async move {
            for l in lines {
                let _ = out_tx.send(l).await;
            }
            for l in errs {
                let _ = err_tx.send(l).await;
            }
            if hold {
                stopped.notified().await;
            }
            drop(out_tx);
            drop(err_tx);
            if wedged {
                // The pipes are shut and the process is not: the only way out
                // is the caller dropping `status`, which this send never wins.
                std::future::pending::<()>().await;
            }
            let _ = done_tx.send(exit);
        });
        Ok(Spawned {
            stdout,
            stderr,
            // The agent runner stops containers by name, never by signal.
            kill: container::KillHandle::detached(),
            status: Box::pin(async move { Ok(done_rx.await.unwrap_or(Exit::Code(-1))) }),
        })
    }

    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "podman");
        self.calls.lock().unwrap().push(Call {
            argv: args.to_vec(),
            mounted: Vec::new(),
        });
        if args.first().map(String::as_str) == Some("stop") {
            if self.wedged {
                return Ok(CmdOutput {
                    status: 125,
                    stdout: String::new(),
                    stderr: "no container with name or ID found".into(),
                });
            }
            // `notify_one`, not `notify_waiters`: the stop may land before the
            // replay task gets to the await, and a lost wake would hang the run.
            self.stopped.notify_one();
        }
        Ok(CmdOutput {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

fn spawn_calls(calls: &Arc<Mutex<Vec<Call>>>) -> Vec<Call> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.argv.first().map(String::as_str) == Some("run"))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// §11 item 2's done-criteria, against the fake
// ---------------------------------------------------------------------------

const HAPPY: [&str; 5] = [
    r#"{"type":"log","message":"listing"}"#,
    "a library printed this and it is not JSON",
    r#"{"type":"progress","done":1,"total":2,"stage":"classifying"}"#,
    r#"{"type":"row","id":"m1","columns":{"subject":"Invoice"},"output":{"category":"Work"}}"#,
    r#"{"type":"row","id":"m2","columns":{"subject":"Sale"},"output":{"category":"Newsletter"},"attention":true}"#,
];

#[tokio::test]
async fn a_run_that_prints_rows_reaches_the_review_table_and_the_env_is_addressing_only() {
    let (state, prefix) = gateway().await;
    let fake = Arc::new(FakeSpawner::new(&HAPPY));
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    let base = serve(state.clone()).await;
    install(
        &base,
        &doc("localhost/labeler:1", 600, r#"["run","apply"]"#, ""),
    )
    .await;

    let job = start(&base, json!({ "id": "labeler", "phase": "run" })).await;
    let d = wait_done(&base, job).await;
    assert_eq!(d["job"]["status"], "done", "{d}");

    // The rows the review table draws from, upserted by id, in order.
    let rows = rows(&d);
    assert_eq!(rows.len(), 2, "{d}");
    assert_eq!(rows[0]["id"], "m1");
    assert_eq!(rows[0]["columns"]["subject"], "Invoice");
    assert_eq!(rows[0]["output"]["category"], "Work");
    assert_eq!(rows[1]["attention"], true);
    assert_eq!(d["result"]["attention"], 1);
    assert_eq!(d["result"]["container"], true);
    // The declared column order, so the header is the author's, not the map's.
    assert_eq!(d["batch"]["columns"], json!(["subject"]));
    assert_eq!(d["batch"]["has_classify"], false);
    assert_eq!(d["batch"]["has_apply"], true);

    // A line that is not JSON is a run-log line verbatim, never a failure.
    let text = log(&d);
    assert!(text.contains("[info] listing"), "{text}");
    assert!(
        text.contains("a library printed this and it is not JSON"),
        "{text}"
    );

    // One invocation, named and labelled, with addressing in env and the two
    // documents on ro,Z mounts.
    let spawns = spawn_calls(&calls);
    assert_eq!(spawns.len(), 1);
    let c = &spawns[0];
    let name = format!("{prefix}-agent-labeler-{job}");
    assert!(c.argv.contains(&name), "{:?}", c.argv);
    assert!(
        c.argv.contains(&"lmgw.kind=agent".to_string())
            && c.argv.contains(&format!("lmgw.instance={prefix}"))
            && c.argv.contains(&format!("lmgw.run={job}")),
        "{:?}",
        c.argv
    );
    assert_eq!(c.env("LMGW_PHASE").as_deref(), Some("run"));
    assert_eq!(c.env("LMGW_RUN"), Some(job.to_string()));
    assert_eq!(c.env("LMGW_SECRETS").as_deref(), Some("/lmgw/secrets.json"));
    assert!(
        c.argv.iter().all(|a| !a.contains("lmgw-agent-")),
        "the token must never reach the argv: {:?}",
        c.argv
    );
    // The mounts, read back off the disk lmgw wrote them to.
    let input = c.mount("/lmgw/input.json");
    assert_eq!(input["phase"], "run");
    assert_eq!(input["config"]["label_prefix"], "ai");
    assert!(input["config"].get("api_token").is_none(), "{input}");
    let secrets = c.mount("/lmgw/secrets.json");
    assert!(
        secrets["token"]
            .as_str()
            .unwrap_or_default()
            .starts_with("lmgw-agent-"),
        "{secrets}"
    );
    // Minted on demand at the start, exactly as §3.1 says.
    let detail = get_json(&base, "/api/agents/labeler").await;
    assert_eq!(detail["token"]["has_value"], true);
    assert_eq!(detail["runtime"]["image"], "localhost/labeler:1");
    assert_eq!(detail["runtime"]["pull"], "never");
    assert_eq!(detail["runtime"]["memory_mb"], 64);
    assert_eq!(detail["runtime"]["deadline_seconds"], 600);
}

#[tokio::test]
async fn the_apply_phase_re_runs_the_same_image_with_the_reviewed_rows() {
    let (state, _prefix) = gateway().await;
    let fake = Arc::new(FakeSpawner::new(&[
        r#"{"type":"log","message":"writing"}"#,
        r#"{"type":"output","output":{"applied":1}}"#,
    ]));
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    let base = serve(state.clone()).await;
    install(
        &base,
        &doc(
            "localhost/labeler:1",
            600,
            r#"["run","apply"]"#,
            &applied_schema(r#"["run","apply"]"#),
        ),
    )
    .await;

    // The review gate: the reviewer's rows, with an override, go in.
    let reviewed = json!([{ "id": "m1", "output": { "category": "Work" } }]);
    let job = start(
        &base,
        json!({ "id": "labeler", "phase": "apply", "rows": reviewed }),
    )
    .await;
    let d = wait_done(&base, job).await;
    assert_eq!(d["job"]["status"], "done", "{d}");
    assert_eq!(d["result"]["output"], json!({ "applied": 1 }));
    // The Runs tab's Applied block reads the same shape an in-process apply
    // writes, so the review surface renders a container's apply unchanged.
    assert_eq!(d["result"]["applied"]["output"], json!({ "applied": 1 }));

    let c = &spawn_calls(&calls)[0];
    assert_eq!(c.env("LMGW_PHASE").as_deref(), Some("apply"));
    let input = c.mount("/lmgw/input.json");
    assert_eq!(input["phase"], "apply");
    // `Row::for_apply`'s shape: the identity and the output fields, and the
    // review columns deliberately not (catalog §2.4, 2026-09-18).
    assert_eq!(input["rows"], json!([{ "id": "m1", "category": "Work" }]));
}

#[tokio::test]
async fn cancel_ends_the_run_canceled_with_the_rows_it_had() {
    let (state, prefix) = gateway().await;
    let fake = Arc::new(FakeSpawner::new(&HAPPY).holds());
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    let base = serve(state.clone()).await;
    install(&base, &doc("localhost/labeler:1", 600, r#"["run"]"#, "")).await;

    let job = start(&base, json!({ "id": "labeler", "phase": "run" })).await;
    // Wait for the rows to be on the live buffer, then cancel.
    for _ in 0..400 {
        if !rows(&get_json(&base, &format!("/api/agents/runs/{job}")).await).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (status, res) = op(&base, "agent_run_cancel", json!({ "id": "labeler" })).await;
    assert_eq!(status, 200, "{res}");

    let d = wait_done(&base, job).await;
    assert_eq!(d["job"]["status"], "canceled", "{d}");
    assert_eq!(rows(&d).len(), 2, "cancel must not be destructive: {d}");
    assert_eq!(d["result"]["rows"][0]["id"], "m1");
    // `podman stop -t <stop_grace_seconds>` against the run's own container.
    let stops: Vec<Vec<String>> = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.argv.first().map(String::as_str) == Some("stop"))
        .map(|c| c.argv.clone())
        .collect();
    assert_eq!(
        stops,
        vec![vec![
            "stop".to_string(),
            "-t".to_string(),
            "1".to_string(),
            format!("{prefix}-agent-labeler-{job}"),
        ]]
    );
}

#[tokio::test]
async fn a_deadline_ends_the_run_failed_with_the_reason_that_names_the_field() {
    let (state, _prefix) = gateway().await;
    let fake = Arc::new(FakeSpawner::new(&HAPPY).holds());
    state.set_agent_spawner_for_tests(fake);
    let base = serve(state.clone()).await;
    install(&base, &doc("localhost/labeler:1", 1, r#"["run"]"#, "")).await;

    let job = start(&base, json!({ "id": "labeler", "phase": "run" })).await;
    let d = wait_done(&base, job).await;
    assert_eq!(d["job"]["status"], "failed", "{d}");
    let err = d["job"]["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("exceeded its deadline of 1s") && err.contains("run.limits.deadline_seconds"),
        "{err}"
    );
    // Failed, not canceled: nobody asked for it to stop — and the rows it did
    // produce are kept all the same.
    assert_eq!(rows(&d).len(), 2, "{d}");
}

#[tokio::test]
async fn an_exit_zero_with_no_output_fails_when_the_manifest_declares_a_schema() {
    let (state, _prefix) = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(FakeSpawner::new(&[r#"{"type":"row","id":"m1"}"#])));
    let base = serve(state.clone()).await;
    install(
        &base,
        &doc(
            "localhost/labeler:1",
            600,
            r#"["run"]"#,
            &applied_schema(r#"["run"]"#),
        ),
    )
    .await;

    let d = wait_done(
        &base,
        start(&base, json!({ "id": "labeler", "phase": "run" })).await,
    )
    .await;
    assert_eq!(d["job"]["status"], "failed", "{d}");
    let err = d["job"]["error"].as_str().unwrap_or_default();
    assert!(err.contains("emitted no output"), "{err}");
    assert!(err.contains("run.output"), "{err}");
}

#[tokio::test]
async fn an_output_that_does_not_match_the_schema_fails_the_run_naming_the_mismatch() {
    let (state, _prefix) = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(FakeSpawner::new(&[
        r#"{"type":"output","output":{"applied":"lots"}}"#,
    ])));
    let base = serve(state.clone()).await;
    install(
        &base,
        &doc(
            "localhost/labeler:1",
            600,
            r#"["run"]"#,
            &applied_schema(r#"["run"]"#),
        ),
    )
    .await;

    let d = wait_done(
        &base,
        start(&base, json!({ "id": "labeler", "phase": "run" })).await,
    )
    .await;
    assert_eq!(d["job"]["status"], "failed", "{d}");
    let err = d["job"]["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("'applied'") && err.contains("run.output"),
        "{err}"
    );
    // The output is still stored: what it reported is what the owner has to see.
    assert_eq!(d["result"]["output"], json!({ "applied": "lots" }));
}

#[tokio::test]
async fn a_nonzero_exit_fails_with_the_status_and_the_stderr_tail_which_is_also_in_the_log() {
    let (state, _prefix) = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(
        FakeSpawner::new(&[r#"{"type":"row","id":"m1"}"#])
            .stderr(&["Traceback (most recent call last)", "KeyError: 'category'"])
            .exit(3),
    ));
    let base = serve(state.clone()).await;
    install(&base, &doc("localhost/labeler:1", 600, r#"["run"]"#, "")).await;

    let d = wait_done(
        &base,
        start(&base, json!({ "id": "labeler", "phase": "run" })).await,
    )
    .await;
    assert_eq!(d["job"]["status"], "failed", "{d}");
    let err = d["job"]["error"].as_str().unwrap_or_default();
    assert!(err.contains("exited with status 3"), "{err}");
    assert!(err.contains("KeyError: 'category'"), "{err}");
    // Verbatim into the run log too, in full rather than as an excerpt.
    assert!(log(&d).contains("Traceback (most recent call last)"), "{d}");
    // And the row it did emit is kept.
    assert_eq!(rows(&d).len(), 1, "{d}");
}

#[tokio::test]
async fn a_phase_the_image_does_not_implement_is_refused_naming_both() {
    let (state, _prefix) = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(FakeSpawner::new(&[])));
    let base = serve(state.clone()).await;
    install(&base, &doc("localhost/labeler:1", 600, r#"["run"]"#, "")).await;

    let (status, res) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "apply" }),
    )
    .await;
    assert_ne!(status, 200, "{res}");
    let msg = res.to_string();
    assert!(
        msg.contains("container agent") && msg.contains("'apply'"),
        "{msg}"
    );
    // And a batch-only phase is refused the same way.
    let (status, res) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "classify" }),
    )
    .await;
    assert_ne!(status, 200, "{res}");
    assert!(res.to_string().contains("classify"), "{res}");
}

#[tokio::test]
async fn a_container_with_no_image_blocks_start_with_the_warnings_code() {
    let (state, _prefix) = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(FakeSpawner::new(&[])));
    let base = serve(state.clone()).await;
    let manifest = r#"{ "schema_version": 1, "id": "svc", "name": "Svc",
        "model": { "alias": "m" },
        "run": { "kind": "container", "service": { "port": 8080 } } }"#;
    install(&base, manifest).await;

    let detail = get_json(&base, "/api/agents/svc").await;
    assert_eq!(
        blocking_codes(&detail),
        ["container_without_image"],
        "{detail}"
    );

    let (status, res) = op(&base, "agent_run", json!({ "id": "svc", "phase": "run" })).await;
    assert_ne!(status, 200, "{res}");
    assert!(res.to_string().contains("container_without_image"), "{res}");
}

/// A spawner podman *answers* for, but whose `run` of the container fails.
///
/// The §3.2 row "podman could not run at all", reached from inside the runner
/// rather than from the Start gate.
struct SpawnFails;

#[async_trait]
impl Spawner for SpawnFails {
    async fn spawn(&self, _p: &str, _a: &[String]) -> std::io::Result<Spawned> {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cannot re-exec process",
        ))
    }
    async fn run(&self, _p: &str, _a: &[String]) -> std::io::Result<CmdOutput> {
        Ok(CmdOutput {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

#[tokio::test]
async fn a_podman_that_cannot_be_run_fails_the_job_with_the_sentence_the_spec_names() {
    let (state, _prefix) = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(SpawnFails));
    let base = serve(state.clone()).await;
    install(&base, &doc("localhost/labeler:1", 600, r#"["run"]"#, "")).await;

    let d = wait_done(
        &base,
        start(&base, json!({ "id": "labeler", "phase": "run" })).await,
    )
    .await;
    assert_eq!(d["job"]["status"], "failed", "{d}");
    let err = d["job"]["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("podman is required for container agents and could not be run"),
        "{err}"
    );
    assert!(err.contains("cannot re-exec process"), "{err}");
}

#[tokio::test]
async fn an_owner_without_podman_is_told_so_before_the_run_starts() {
    // `NoSpawner` is what `init_for_tests` installs, and it is also what a box
    // without podman behaves like: the Start gate answers first, so the owner
    // gets the named warning rather than a failed job row.
    let (state, _prefix) = gateway().await;
    let base = serve(state.clone()).await;
    install(&base, &doc("localhost/labeler:1", 600, r#"["run"]"#, "")).await;

    let detail = get_json(&base, "/api/agents/labeler").await;
    assert_eq!(blocking_codes(&detail), ["podman_unavailable"], "{detail}");
    assert_eq!(detail["runtime"]["podman"], false, "{detail}");

    let (status, res) = op(
        &base,
        "agent_run",
        json!({ "id": "labeler", "phase": "run" }),
    )
    .await;
    assert_ne!(status, 200, "{res}");
    let msg = res.to_string();
    assert!(
        msg.contains("podman is required for container agents"),
        "{msg}"
    );
    assert!(msg.contains("podman_unavailable"), "{msg}");
}

#[tokio::test]
async fn a_stop_that_does_not_bite_still_ends_the_run_with_the_rows_it_had() {
    // The wedge the review found: the container shuts its pipes, never exits,
    // and `podman stop` fails (it was still being created, or the name lost the
    // race). Every rung of the ladder is climbed and the last one — dropping
    // the child — is what ends the run, inside the *visible* grace window.
    let (state, _prefix) = gateway().await;
    let fake = Arc::new(FakeSpawner::new(&HAPPY).wedged());
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    let base = serve(state.clone()).await;
    install(&base, &doc("localhost/labeler:1", 600, r#"["run"]"#, "")).await;

    let job = start(&base, json!({ "id": "labeler", "phase": "run" })).await;
    for _ in 0..400 {
        if !rows(&get_json(&base, &format!("/api/agents/runs/{job}")).await).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let started = std::time::Instant::now();
    let (status, res) = op(&base, "agent_run_cancel", json!({ "id": "labeler" })).await;
    assert_eq!(status, 200, "{res}");

    let d = wait_done(&base, job).await;
    assert_eq!(d["job"]["status"], "canceled", "{d}");
    assert_eq!(rows(&d).len(), 2, "cancel must not be destructive: {d}");
    // Two settle windows of `stop_grace_seconds` (1s here) plus the polls, and
    // nothing longer: no rung waits on a bound nobody can see.
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "the ladder took {:?}",
        started.elapsed()
    );

    // Each rung said what it did and why, in the run log.
    let text = log(&d);
    assert!(text.contains("podman stop -t 1"), "{text}");
    assert!(text.contains("podman rm -f"), "{text}");
    assert!(text.contains("killing the podman run process"), "{text}");
    // And the escalation really was three steps, not one warn-and-hope.
    let verbs: Vec<String> = calls
        .lock()
        .unwrap()
        .iter()
        .filter_map(|c| c.argv.first().cloned())
        .collect();
    assert!(verbs.contains(&"stop".to_string()), "{verbs:?}");
}

#[tokio::test]
async fn a_cancel_during_the_deadline_sequence_is_seen_and_the_first_cause_keeps_the_run() {
    let (state, _prefix) = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(FakeSpawner::new(&HAPPY).wedged()));
    let base = serve(state.clone()).await;
    install(&base, &doc("localhost/labeler:1", 1, r#"["run"]"#, "")).await;

    let job = start(&base, json!({ "id": "labeler", "phase": "run" })).await;
    // After the 1 s deadline has fired and while the ladder is still climbing
    // (two 1 s settles): the old code gated the cancel poll off here.
    tokio::time::sleep(Duration::from_millis(1400)).await;
    let (status, res) = op(&base, "agent_run_cancel", json!({ "id": "labeler" })).await;
    assert_eq!(status, 200, "{res}");

    let d = wait_done(&base, job).await;
    // The deadline got there first and keeps the run's reason: saying it was
    // cancelled would hide why it was already over.
    assert_eq!(d["job"]["status"], "failed", "{d}");
    assert!(
        d["job"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("exceeded its deadline"),
        "{d}"
    );
    // Observed all the same, which is the part that was broken.
    assert!(
        log(&d).contains("cancel requested while the container was already being stopped"),
        "{}",
        log(&d)
    );
    assert_eq!(rows(&d).len(), 2, "{d}");
}

#[tokio::test]
async fn a_nonzero_exit_after_a_cancel_is_canceled_not_failed() {
    // jobs/mod.rs says it plainly: cancel must not be destructive, and a
    // container that exits non-zero *because it was stopped* has not failed.
    let (state, _prefix) = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(FakeSpawner::new(&HAPPY).holds().exit(137)));
    let base = serve(state.clone()).await;
    install(&base, &doc("localhost/labeler:1", 600, r#"["run"]"#, "")).await;

    let job = start(&base, json!({ "id": "labeler", "phase": "run" })).await;
    for _ in 0..400 {
        if !rows(&get_json(&base, &format!("/api/agents/runs/{job}")).await).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    op(&base, "agent_run_cancel", json!({ "id": "labeler" })).await;

    let d = wait_done(&base, job).await;
    assert_eq!(d["job"]["status"], "canceled", "{d}");
    assert!(d["job"]["error"].is_null(), "{d}");
    assert_eq!(rows(&d).len(), 2, "{d}");
}

#[tokio::test]
async fn the_run_log_is_live_while_the_container_is_still_talking() {
    // `AgentRunDetail.log` is what the Run tab renders; transport A keeps its
    // accumulator off the ledger desk, so without the buffer's log slot every
    // line would only appear once the job had ended.
    let (state, _prefix) = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(FakeSpawner::new(&HAPPY).holds()));
    let base = serve(state.clone()).await;
    install(&base, &doc("localhost/labeler:1", 600, r#"["run"]"#, "")).await;

    let job = start(&base, json!({ "id": "labeler", "phase": "run" })).await;
    let mut seen = String::new();
    for _ in 0..400 {
        let d = get_json(&base, &format!("/api/agents/runs/{job}")).await;
        seen = log(&d);
        if seen.contains("[info] listing") {
            assert_eq!(d["job"]["status"], "running", "still in flight: {d}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(seen.contains("[info] listing"), "{seen}");
    assert!(
        seen.contains("a library printed this and it is not JSON"),
        "{seen}"
    );
    op(&base, "agent_run_cancel", json!({ "id": "labeler" })).await;
    wait_done(&base, job).await;
}

#[tokio::test]
async fn the_apply_ceiling_is_the_tools_the_token_can_actually_reach() {
    // `allowed: None` means *the whole label* — what `ToolSurface::allowed`
    // resolves and what `/mcp` filters on. Deriving the ceiling from `allowed`
    // alone printed nothing at all for this, the commonest manifest shape.
    let (state, _prefix) = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(FakeSpawner::new(&[])));
    let base = serve(state.clone()).await;
    let manifest = r#"{ "schema_version": 1, "id": "labeler", "name": "L",
        "model": { "alias": "m" },
        "tools": [ { "label": "lmgw" } ],
        "run": { "kind": "container", "image": "localhost/l:1",
                 "phases": ["run", "apply"] } }"#;
    install(&base, manifest).await;

    let detail = get_json(&base, "/api/agents/labeler").await;
    let tools: Vec<String> = detail["batch"]["apply_tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert!(!tools.is_empty(), "an empty ceiling is the bug: {detail}");
    assert!(tools.iter().any(|t| t == "lmgw__status"), "{tools:?}");
    assert_eq!(detail["batch"]["apply_tools_are_ceiling"], true, "{detail}");
}

// ---------------------------------------------------------------------------
// Boot reconciliation (§6.4)
// ---------------------------------------------------------------------------

/// "This process started after everything already on the box" — what a boot is,
/// from the point of view of the leftovers it collects. A test cannot backdate
/// a directory it just made or a container podman just created, so it moves the
/// other end of the comparison instead (container-runtime §6.4).
fn after_everything() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() + chrono::Duration::seconds(5)
}

/// A `CommandRunner` that answers `ps` with a scripted listing and records
/// every other verb.
struct FakePodman {
    ps: String,
    seen: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl CommandRunner for FakePodman {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "podman");
        self.seen.lock().unwrap().push(args.to_vec());
        let stdout = if args.first().map(String::as_str) == Some("ps") {
            self.ps.clone()
        } else {
            String::new()
        };
        Ok(CmdOutput {
            status: 0,
            stdout,
            stderr: String::new(),
        })
    }
}

#[tokio::test]
async fn reconciliation_collects_a_leftover_container_and_its_run_directory() {
    let (state, prefix) = gateway().await;
    // A container this instance left behind, and one belonging to a different
    // instance that the label filter would never have listed anyway.
    let leftover = format!("{prefix}-agent-labeler-7");
    let ps = json!([
        { "Names": [leftover], "State": "running",
          "Labels": { "lmgw.kind": "agent", "lmgw.instance": prefix, "lmgw.run": "7",
                      "lmgw.agent": "labeler" } },
        { "Names": [format!("{prefix}-agent-other-9")], "State": "exited",
          "Labels": { "lmgw.kind": "agent", "lmgw.instance": prefix, "lmgw.run": "nine" } },
    ])
    .to_string();
    let seen = Arc::new(Mutex::new(Vec::new()));
    state.set_runtime_for_tests(Arc::new(Registry::new(
        Arc::new(FakePodman {
            ps,
            seen: seen.clone(),
        }),
        reqwest::Client::new(),
    )));

    // A run directory of a run that is not live either.
    let (root, _) = container::runs_root(&state.data_dir, &prefix);
    let dir = container::RunDir::create(&root, 7).unwrap();
    let path = dir.path().to_path_buf();
    std::mem::forget(dir); // the sweep is what must remove it, not the Drop.
    assert!(path.exists());

    let report = container::reconcile_since(&state, after_everything()).await;
    assert!(report.listed, "{report:?}");
    // Both: the second one's `lmgw.run` label is not a job id at all, and
    // nothing can vouch for a container whose run cannot be named.
    assert_eq!(
        report.removed,
        vec![leftover.clone(), format!("{prefix}-agent-other-9")],
        "{report:?}"
    );
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(
        report.swept_dirs.contains(&"run-7".to_string()),
        "{report:?}"
    );
    assert!(!path.exists());

    // The filter is the label pair, and the removal is `rm -f` by name.
    let calls = seen.lock().unwrap().clone();
    assert!(
        calls[0].contains(&"label=lmgw.kind=agent".to_string())
            && calls[0].contains(&format!("label=lmgw.instance={prefix}")),
        "{calls:?}"
    );
    assert!(
        calls.iter().any(|c| c == &["rm", "-f", &leftover]),
        "{calls:?}"
    );
    // A container is never adopted, only collected — and one whose run label is
    // not a number is collected too, since nothing can vouch for it.
    assert_eq!(
        calls.iter().filter(|c| c[0] == "rm").count(),
        2,
        "{calls:?}"
    );
}

/// A package read that was killed halfway leaves a `lmgw-pkg-*` container and a
/// `pkg-*` run directory (container-runtime §3.4). Both are labelled and named
/// so boot reconciliation can see them — without the label the container would
/// be invisible to the filter, and without the prefix the sweep would skip the
/// directory forever.
#[tokio::test]
async fn reconciliation_collects_a_half_finished_package_read() {
    let (state, prefix) = gateway().await;
    let leftover = "lmgw-pkg-0123456789abcdef".to_string();
    let ps = json!([
        { "Names": [leftover.clone()], "State": "created",
          "Labels": { "lmgw.kind": "agent", "lmgw.instance": prefix,
                      "lmgw.run": "package" } },
    ])
    .to_string();
    let seen = Arc::new(Mutex::new(Vec::new()));
    state.set_runtime_for_tests(Arc::new(Registry::new(
        Arc::new(FakePodman {
            ps,
            seen: seen.clone(),
        }),
        reqwest::Client::new(),
    )));

    let (root, _) = container::runs_root(&state.data_dir, &prefix);
    let dir = container::RunDir::create_named(&root, &format!("pkg-{leftover}")).unwrap();
    let path = dir.path().to_path_buf();
    std::mem::forget(dir); // the sweep is what must remove it, not the Drop.
    assert!(path.exists());

    let report = container::reconcile_since(&state, after_everything()).await;
    // `package` is not a job id, which is exactly the "collect it" answer a run
    // label that cannot be parsed gets — the same reading `service` gets.
    assert_eq!(report.removed, vec![leftover.clone()], "{report:?}");
    assert!(
        report.swept_dirs.contains(&format!("pkg-{leftover}")),
        "{report:?}"
    );
    assert!(!path.exists());
    assert!(report.errors.is_empty(), "{report:?}");
}

// ---------------------------------------------------------------------------
// Mounts (§5.5–§5.7), against the fake
// ---------------------------------------------------------------------------

/// A container agent with one `rw` directory slot and one ordinary field, so a
/// run can be told to use a different folder *and* a different value for one
/// click without saving either.
fn mount_doc(image: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "notes-desk",
  "name": "Notes desk",
  "model": {{ "alias": "m1" }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "notes": {{ "type": "string", "format": "directory", "access": "rw" }},
    "label_prefix": {{ "type": "string", "default": "ai" }}
  }} }} }},
  "run": {{
    "kind": "container",
    "image": "{image}",
    "columns": ["subject"],
    "review": {{ "editable": ["category"] }},
    "phases": ["run", "apply"],
    "limits": {{ "memory_mb": 64, "cpus": 1.0, "pids": 32,
                "deadline_seconds": 600, "stop_grace_seconds": 1 }}
  }}
}}"#
    )
}

/// A folder for a mount to point at, under `$TMPDIR` and **never** under
/// `$HOME`: binding relabels what it is given, recursively and for good.
fn temp_folder() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temp dir")
}

fn canonical(dir: &tempfile::TempDir) -> String {
    std::fs::canonicalize(dir.path())
        .expect("the folder is there")
        .display()
        .to_string()
}

/// Every `-v` argument of one invocation, in argv order.
fn mount_flags(c: &Call) -> Vec<String> {
    c.argv
        .iter()
        .enumerate()
        .filter(|(i, _)| *i > 0 && c.argv[i - 1] == "-v")
        .map(|(_, a)| a.clone())
        .collect()
}

/// The whole of §5.5 and §5.6 on one run: the `-v` line with its shared label,
/// `keep-id` because the manifest declares a slot, the two documents the owner
/// never sees a host path in, and the run log that says what binding did.
#[tokio::test]
async fn a_bound_mount_reaches_the_argv_the_documents_and_the_run_log() {
    let (state, _prefix) = gateway().await;
    let fake = Arc::new(FakeSpawner::new(&HAPPY));
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    let base = serve(state.clone()).await;
    install(&base, &mount_doc("localhost/notes:1")).await;
    let folder = temp_folder();
    let notes = canonical(&folder);
    let (status, v) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": notes } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");

    let job = start(&base, json!({ "id": "notes-desk", "phase": "run" })).await;
    let d = wait_done(&base, job).await;
    assert_eq!(d["job"]["status"], "done", "{d}");

    let spawns = spawn_calls(&calls);
    assert_eq!(spawns.len(), 1);
    let c = &spawns[0];
    // lmgw's own files stay private; the owner's folder is shared, because two
    // containers on one folder is the ordinary case (§5.5).
    let flags = mount_flags(c);
    assert!(
        flags[0].ends_with("/lmgw/input.json:ro,Z")
            && flags[1].ends_with("/lmgw/secrets.json:ro,Z"),
        "{flags:?}"
    );
    assert_eq!(
        flags[2],
        format!("{notes}:/lmgw/mounts/notes:rw,z"),
        "{flags:?}"
    );
    assert!(
        c.argv.iter().any(|a| a == "--userns=keep-id"),
        "{:?}",
        c.argv
    );

    // `input.json`: the container path, the array beside it, and no host path
    // anywhere in the document.
    let input = c.mount("/lmgw/input.json");
    assert_eq!(input["config"]["notes"], "/lmgw/mounts/notes", "{input}");
    assert_eq!(
        input["mounts"],
        json!([{ "field": "notes", "path": "/lmgw/mounts/notes",
                 "kind": "directory", "access": "rw" }]),
        "{input}"
    );
    assert!(!input.to_string().contains(&notes), "{input}");

    // The run log, in §5.6's order: the mount line, then the start line, which
    // names the uid the container will run as.
    let text = log(&d);
    let uid = std::fs::metadata("/proc/self")
        .map(|m| std::os::unix::fs::MetadataExt::uid(&m))
        .unwrap();
    let mount_line = format!(
        "mount notes: {notes} → /lmgw/mounts/notes (rw, directory; relabelled container_file_t, \
         recursive, permanent)"
    );
    assert!(text.contains(&mount_line), "{text}");
    let start_line = format!(
        "this manifest declares mounts: the container runs as uid {uid} (--userns=keep-id)"
    );
    assert!(text.contains(&start_line), "{text}");
    let lines: Vec<&str> = text.lines().collect();
    let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
    assert!(
        at("mount notes:") < at("starting localhost/notes:1"),
        "{text}"
    );
}

/// `--userns=keep-id` is the **manifest's** property: an unbound slot still
/// declares one, so the image runs the one way either way — and an agent that
/// declares none sees no change at all (§5.5).
#[tokio::test]
async fn keep_id_follows_the_declaration_and_not_the_binding() {
    let (state, _prefix) = gateway().await;
    let fake = Arc::new(FakeSpawner::new(&HAPPY));
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    let base = serve(state.clone()).await;

    // Declared, and nobody has bound it.
    install(&base, &mount_doc("localhost/notes:1")).await;
    let job = start(&base, json!({ "id": "notes-desk", "phase": "run" })).await;
    assert_eq!(wait_done(&base, job).await["job"]["status"], "done");
    let c = spawn_calls(&calls).pop().unwrap();
    assert!(
        c.argv.iter().any(|a| a == "--userns=keep-id"),
        "{:?}",
        c.argv
    );
    assert!(
        !c.argv.iter().any(|a| a.contains("/lmgw/mounts/")),
        "nothing is bound, so nothing is mounted: {:?}",
        c.argv
    );
    assert_eq!(c.mount("/lmgw/input.json")["mounts"], json!([]));

    // Not declared: the flag is absent, exactly as it was before this part.
    install(
        &base,
        &doc("localhost/labeler:1", 600, r#"["run","apply"]"#, ""),
    )
    .await;
    let job = start(&base, json!({ "id": "labeler", "phase": "run" })).await;
    assert_eq!(wait_done(&base, job).await["job"]["status"], "done");
    let c = spawn_calls(&calls).pop().unwrap();
    assert!(
        !c.argv.iter().any(|a| a == "--userns=keep-id"),
        "an existing manifest sees no change: {:?}",
        c.argv
    );
}

/// The use-time half of §5.3: a folder that was there when it was saved and is
/// gone when the run starts fails the run with `mount_path_missing`, and podman
/// is never reached.
#[tokio::test]
async fn a_folder_that_went_away_fails_the_start_before_podman() {
    let (state, _prefix) = gateway().await;
    let fake = Arc::new(FakeSpawner::new(&HAPPY));
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    let base = serve(state.clone()).await;
    install(&base, &mount_doc("localhost/notes:1")).await;
    let folder = temp_folder();
    let notes = canonical(&folder);
    let (status, v) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": notes } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");

    // The filesystem moves between the save and the start — that is the whole
    // reason the rules are checked twice.
    std::fs::remove_dir_all(folder.path()).unwrap();
    let job = start(&base, json!({ "id": "notes-desk", "phase": "run" })).await;
    let d = wait_done(&base, job).await;
    assert_eq!(d["job"]["status"], "failed", "{d}");
    let err = d["job"]["error"].as_str().unwrap_or_default();
    assert!(err.contains("mount_path_missing"), "{d}");
    assert!(err.contains("notes:"), "the field is named: {d}");
    assert!(
        spawn_calls(&calls).is_empty(),
        "podman must not be reached: {:?}",
        spawn_calls(&calls)
    );
}

/// The other two use-time refusals (§5.3, §5.9). `mount_path_missing` is the
/// one a folder that was renamed produces; these two are what a **symlink**
/// produces, which is the case the rules are checked twice for in the first
/// place — the value never changed, what it resolves to did.
#[tokio::test]
async fn a_symlink_repointed_after_the_save_is_refused_at_use_by_rule_4_and_rule_5() {
    let (state, _prefix) = gateway().await;
    let fake = Arc::new(FakeSpawner::new(&HAPPY));
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    let base = serve(state.clone()).await;
    install(&base, &mount_doc("localhost/notes:1")).await;

    let folder = temp_folder();
    let root = std::fs::canonicalize(folder.path()).unwrap();
    let real = root.join("Notes");
    let link = root.join("notes-link");
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    async fn bind(base: &Gw, id: &str, value: &str) -> (u16, Value) {
        op(
            base,
            "agent_config_set",
            json!({ "id": id, "values": { "notes": value } }),
        )
        .await
    }
    async fn fail(base: &Gw, job: i64) -> String {
        let d = wait_done(base, job).await;
        assert_eq!(d["job"]["status"], "failed", "{d}");
        d["job"]["error"].as_str().unwrap_or_default().to_string()
    }

    // Rule 4 at use time: the link now lands in a place rule 4 names. The
    // stored value is a canonical path — `/etc` is reached by putting a link
    // in its place, which is exactly the swap §5.3's race paragraph is about.
    let (status, v) = bind(&base, "notes-desk", &link.display().to_string()).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["config"]["notes"],
        json!(real.display().to_string()),
        "{v}"
    );
    std::fs::remove_dir(&real).unwrap();
    std::os::unix::fs::symlink("/etc", &real).unwrap();
    let job = start(&base, json!({ "id": "notes-desk", "phase": "run" })).await;
    let err = fail(&base, job).await;
    assert!(err.contains("mount_path_refused"), "{err}");
    assert!(err.contains("not a data directory"), "{err}");
    assert!(err.starts_with("notes: "), "the field is named: {err}");
    assert!(spawn_calls(&calls).is_empty(), "podman must not be reached");

    // Rule 5 at use time: a second agent holds a tree `rw`, and this one's
    // path now resolves inside it. Neither value changed.
    std::fs::remove_file(&real).unwrap();
    std::fs::create_dir_all(&real).unwrap();
    let (status, v) = bind(&base, "notes-desk", &real.display().to_string()).await;
    assert_eq!(status, 200, "{v}");
    let theirs = root.join("Archive");
    let inside = theirs.join("daily");
    std::fs::create_dir_all(&inside).unwrap();
    install(
        &base,
        &mount_doc("localhost/notes:1").replace("notes-desk", "archivist"),
    )
    .await;
    let (status, v) = bind(&base, "archivist", &theirs.display().to_string()).await;
    assert_eq!(status, 200, "{v}");
    std::fs::remove_dir(&real).unwrap();
    std::os::unix::fs::symlink(&inside, &real).unwrap();

    let job = start(&base, json!({ "id": "notes-desk", "phase": "run" })).await;
    let err = fail(&base, job).await;
    assert!(err.contains("mount_path_nested"), "{err}");
    assert!(
        err.contains("'archivist'"),
        "the other agent is named: {err}"
    );
    assert!(
        err.contains(" rw on 'notes'"),
        "with its field and access: {err}"
    );
    assert!(spawn_calls(&calls).is_empty(), "podman must not be reached");
}

/// A per-run mount override is judged before the job row exists (§5.7): it is
/// never written, so the **store** rules run against the patch at the click
/// rather than producing a job that exists only to fail.
#[tokio::test]
async fn a_per_run_mount_override_is_refused_at_the_click_and_starts_nothing() {
    let (state, _prefix) = gateway().await;
    let fake = Arc::new(FakeSpawner::new(&HAPPY));
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    let base = serve(state.clone()).await;
    install(&base, &mount_doc("localhost/notes:1")).await;
    let folder = temp_folder();
    let notes = canonical(&folder);
    let (status, v) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": notes } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");

    for (value, code, needle) in [
        ("/etc", "mount_path_refused", "not a data directory"),
        ("Notes", "mount_path_refused", "absolute path"),
        (
            "/nowhere/at/all",
            "mount_path_refused",
            "clear the field or point it at a folder that exists",
        ),
    ] {
        let (status, v) = op(
            &base,
            "agent_run",
            json!({ "id": "notes-desk", "phase": "run", "values": { "notes": value } }),
        )
        .await;
        assert_eq!(status, 400, "{value}: {v}");
        let message = v["message"].as_str().unwrap_or_default();
        assert!(message.contains(code), "{value}: {v}");
        assert!(message.contains(needle), "{value}: {v}");
    }
    assert!(
        spawn_calls(&calls).is_empty(),
        "nothing was started: {:?}",
        spawn_calls(&calls)
    );
    // And no job row was created to carry the refusal.
    let runs = get_json(&base, "/api/agents/notes-desk/runs").await;
    assert_eq!(runs.as_array().map(Vec::len), Some(0), "{runs}");
}

/// §5.7, the general fix: an apply reruns against **the run's** config, not
/// against whatever the stored config says by the time the reviewer presses
/// Apply. The Apply button sends `base_job` and no values at all.
#[tokio::test]
async fn an_apply_runs_against_the_config_its_run_used() {
    let (state, _prefix) = gateway().await;
    let fake = Arc::new(FakeSpawner::new(&HAPPY));
    let calls = fake.calls();
    state.set_agent_spawner_for_tests(fake);
    let base = serve(state.clone()).await;
    install(&base, &mount_doc("localhost/notes:1")).await;
    let saved = temp_folder();
    let once = temp_folder();
    let (saved, once) = (canonical(&saved), canonical(&once));
    let (status, v) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": saved } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");

    // A run against a different folder and a different label, neither saved.
    let job = start(
        &base,
        json!({ "id": "notes-desk", "phase": "run",
                "values": { "notes": once, "label_prefix": "just-this-once" } }),
    )
    .await;
    assert_eq!(wait_done(&base, job).await["job"]["status"], "done");
    let run_call = spawn_calls(&calls).pop().unwrap();
    assert!(
        mount_flags(&run_call).contains(&format!("{once}:/lmgw/mounts/notes:rw,z")),
        "{:?}",
        run_call.argv
    );

    // Apply, sending nothing but the run it is applying — which is exactly
    // what the Apply button sends.
    let apply = start(
        &base,
        json!({ "id": "notes-desk", "phase": "apply", "base_job": job,
                "rows": [{ "id": "m1", "output": { "category": "Work" } }] }),
    )
    .await;
    assert_eq!(wait_done(&base, apply).await["job"]["status"], "done");
    let c = spawn_calls(&calls).pop().unwrap();
    assert!(
        mount_flags(&c).contains(&format!("{once}:/lmgw/mounts/notes:rw,z")),
        "the apply wrote into the folder the run read: {:?}",
        c.argv
    );
    assert_eq!(
        c.mount("/lmgw/input.json")["config"]["label_prefix"],
        "just-this-once",
        "and the rest of that run's config came with it"
    );

    // `effective` holds host paths, and the runs routes are read by the agent
    // itself (principals §3.2) — so it must not reach them, and it does not: a
    // summary is built from `agent_id` and `phase` and nothing else of the
    // job's input.
    let list = get_json(&base, "/api/agents/notes-desk/runs")
        .await
        .to_string();
    assert!(!list.contains(&once) && !list.contains(&saved), "{list}");
    for id in [job, apply] {
        let d = get_json(&base, &format!("/api/agents/runs/{id}")).await;
        for part in ["job", "rows", "batch"] {
            let body = d[part].to_string();
            assert!(
                !body.contains(&once) && !body.contains(&saved),
                "run {id}'s {part} carries a host path: {body}"
            );
        }
        // The one place a host path *is* in a run's document is the mount line
        // §5.6 requires the run log to print — principle 4, and §3.10's
        // reading that a run's log is that run's own output, diagnostic
        // sentences and all.
        assert!(
            d["log"].to_string().contains(&once),
            "the run log names what was bound: {d}"
        );
    }
}

/// A job row written before `effective` existed still reads, and an apply
/// started from it behaves as it always did (§5.7).
#[test]
fn a_job_row_without_effective_still_deserialises() {
    let old: lmgw_core::agents::batch::Input = serde_json::from_str(
        r#"{ "agent_id": "notes-desk", "phase": "apply", "values": { "label_prefix": "ai" } }"#,
    )
    .expect("an older row still reads");
    assert!(old.effective.is_none());
    assert_eq!(old.values["label_prefix"], "ai");
}

// ---------------------------------------------------------------------------
// The real thing (needs podman)
// ---------------------------------------------------------------------------

/// The base the test image is built on. Already present on a Fedora box; the
/// build is `COPY` + `ENTRYPOINT` over it and takes well under a second.
const BASE_IMAGE: &str = "registry.fedoraproject.org/fedora-minimal:44";

fn podman_available() -> bool {
    std::process::Command::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn image_present(image: &str) -> bool {
    std::process::Command::new("podman")
        .args(["image", "exists", image])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Removes the image it built, whatever happens — including on a panic, which
/// is the case a plain call at the end of the test would miss.
struct BuiltImage(String);

impl BuiltImage {
    /// Build a one-layer image whose entrypoint is `script`.
    fn new(tag: &str, script: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lmgw-agent-image-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("run.sh"), script).unwrap();
        std::fs::write(
            dir.join("Containerfile"),
            format!("FROM {BASE_IMAGE}\nCOPY run.sh /lmgw-run.sh\nENTRYPOINT [\"/bin/sh\", \"/lmgw-run.sh\"]\n"),
        )
        .unwrap();
        let image = format!("localhost/lmgw-agent-test:{tag}");
        let out = std::process::Command::new("podman")
            .args(["build", "-q", "-t", &image, "-f", "Containerfile", "."])
            .current_dir(&dir)
            .output()
            .expect("podman build runs");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.status.success(),
            "podman build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Self(image)
    }
    fn name(&self) -> &str {
        &self.0
    }
}

impl Drop for BuiltImage {
    fn drop(&mut self) {
        let _ = std::process::Command::new("podman")
            .args(["rmi", "-f", &self.0])
            .output();
    }
}

/// Every container the run may have left, removed by name whatever happened.
struct ContainerSweep(String);

impl Drop for ContainerSweep {
    fn drop(&mut self) {
        let _ = std::process::Command::new("podman")
            .args([
                "rm",
                "-f",
                "--filter",
                &format!("label=lmgw.instance={}", self.0),
            ])
            .output();
        // `rm -f --filter` is podman 5's shape; fall back to a listing sweep so
        // a different podman still leaves nothing behind.
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

const REAL_RUN_SCRIPT: &str = r#"#!/bin/sh
echo "{\"type\":\"log\",\"message\":\"base $LMGW_BASE_URL\"}"
echo "starting up, and this line is not JSON"
if [ "$LMGW_PHASE" = "apply" ]; then
  echo "{\"type\":\"output\",\"output\":{\"applied\":2}}"
  exit 0
fi
cat /lmgw/secrets.json > /tmp/s.json
echo "{\"type\":\"row\",\"id\":\"m1\",\"columns\":{\"subject\":\"Invoice\"},\"output\":{\"category\":\"Work\"}}"
echo "{\"type\":\"row\",\"id\":\"m2\",\"columns\":{\"subject\":\"Sale\"},\"output\":{\"category\":\"Newsletter\"}}"
echo "{\"type\":\"output\",\"output\":{\"applied\":0}}"
"#;

#[tokio::test]
async fn the_real_thing_runs_reviews_and_applies_a_hand_built_image() {
    if !podman_available() {
        eprintln!("SKIP the_real_thing_runs…: podman is not available on this box");
        return;
    }
    if !image_present(BASE_IMAGE) {
        eprintln!("SKIP the_real_thing_runs…: {BASE_IMAGE} is not on this box");
        return;
    }
    let (state, prefix) = gateway().await;
    let _sweep = ContainerSweep(prefix.clone());
    let image = BuiltImage::new(&format!("run{}", std::process::id()), REAL_RUN_SCRIPT);
    state.set_agent_spawner_for_tests(Arc::new(container::TokioSpawner));
    let base = serve(state.clone()).await;
    install(
        &base,
        &doc(
            image.name(),
            120,
            r#"["run","apply"]"#,
            &applied_schema(r#"["run","apply"]"#),
        ),
    )
    .await;

    // --- run: a green review table ---
    let d = wait_done(
        &base,
        start(&base, json!({ "id": "labeler", "phase": "run" })).await,
    )
    .await;
    assert_eq!(d["job"]["status"], "done", "{d}");
    let rows = rows(&d);
    assert_eq!(rows.len(), 2, "{d}");
    assert_eq!(rows[0]["columns"]["subject"], "Invoice");
    assert_eq!(rows[1]["output"]["category"], "Newsletter");
    assert!(rows.iter().all(|r| r["attention"] == json!(false)), "{d}");
    let text = log(&d);
    // The env really reached the container, and the read-only rootfs really had
    // a writable /tmp and a readable secrets mount (the script uses both).
    // The gateway here is bound to the default loopback address, so the base
    // url is the one pasta forwards rather than `host.containers.internal`
    // (container-runtime §3.1, corrected in WP3 against a real container).
    assert!(text.contains("base http://127.0.0.1:"), "{text}");
    assert!(
        text.contains("starting up, and this line is not JSON"),
        "{text}"
    );

    // --- apply: the same image, LMGW_PHASE=apply, an applied result ---
    let reviewed = json!([{ "id": "m1", "output": { "category": "Work" } }]);
    let d = wait_done(
        &base,
        start(
            &base,
            json!({ "id": "labeler", "phase": "apply", "rows": reviewed }),
        )
        .await,
    )
    .await;
    assert_eq!(d["job"]["status"], "done", "{d}");
    assert_eq!(d["result"]["output"], json!({ "applied": 2 }));

    // Nothing is left behind: `--rm` collected the containers and the run
    // directory went with the run.
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

const REAL_SLEEP_SCRIPT: &str = r#"#!/bin/sh
echo "{\"type\":\"row\",\"id\":\"m1\",\"columns\":{\"subject\":\"Invoice\"}}"
sleep 300
"#;

#[tokio::test]
async fn the_real_thing_cancels_with_podman_stop_and_keeps_its_rows() {
    if !podman_available() || !image_present(BASE_IMAGE) {
        eprintln!("SKIP the_real_thing_cancels…: podman or the base image is not available");
        return;
    }
    let (state, prefix) = gateway().await;
    let _sweep = ContainerSweep(prefix.clone());
    let image = BuiltImage::new(&format!("cancel{}", std::process::id()), REAL_SLEEP_SCRIPT);
    state.set_agent_spawner_for_tests(Arc::new(container::TokioSpawner));
    let base = serve(state.clone()).await;
    install(&base, &doc(image.name(), 120, r#"["run"]"#, "")).await;

    let job = start(&base, json!({ "id": "labeler", "phase": "run" })).await;
    for _ in 0..600 {
        if !rows(&get_json(&base, &format!("/api/agents/runs/{job}")).await).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (status, res) = op(&base, "agent_run_cancel", json!({ "id": "labeler" })).await;
    assert_eq!(status, 200, "{res}");

    let d = wait_done(&base, job).await;
    assert_eq!(d["job"]["status"], "canceled", "{d}");
    assert_eq!(rows(&d).len(), 1, "cancel must not be destructive: {d}");
}

#[tokio::test]
async fn the_real_thing_has_its_leftover_container_collected_at_reconciliation() {
    if !podman_available() || !image_present(BASE_IMAGE) {
        eprintln!("SKIP the_real_thing_has_its_leftover…: podman is not available");
        return;
    }
    let (state, prefix) = gateway().await;
    let _sweep = ContainerSweep(prefix.clone());
    // A container carrying exactly the labels a crashed run would have left,
    // started detached so it is still there when reconciliation looks.
    let name = container::container_name(&prefix, "labeler", 4242);
    let out = std::process::Command::new("podman")
        .args([
            "run",
            "-d",
            "--replace",
            "--name",
            &name,
            "--label",
            "lmgw.kind=agent",
            "--label",
            &format!("lmgw.instance={prefix}"),
            "--label",
            "lmgw.agent=labeler",
            "--label",
            "lmgw.run=4242",
            BASE_IMAGE,
            "/bin/sh",
            "-c",
            "sleep 300",
        ])
        .output()
        .expect("podman run");
    assert!(
        out.status.success(),
        "podman run: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The registry the reconciliation reads through is the real one here.
    state.set_runtime_for_tests(Arc::new(Registry::new(
        Arc::new(lmgw_core::runtime::registry::TokioRunner),
        reqwest::Client::new(),
    )));
    // No job is live — `fail_orphaned_jobs` has closed every one a restart
    // interrupted — so the container is a leftover by definition.
    let report = container::reconcile_since(&state, after_everything()).await;
    assert!(report.listed, "{report:?}");
    assert!(report.removed.contains(&name), "{report:?}");
    assert!(report.errors.is_empty(), "{report:?}");

    let still_there = std::process::Command::new("podman")
        .args(["container", "exists", &name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(!still_there, "{name} survived reconciliation");
}

// ---------------------------------------------------------------------------
// Sweeping both roots, and not sweeping what this process started (§6.4)
// ---------------------------------------------------------------------------

/// A `podman ps` listing with no containers at all, so a sweep test is only
/// about directories.
fn empty_ps(seen: Arc<Mutex<Vec<Vec<String>>>>) -> Arc<Registry> {
    Arc::new(Registry::new(
        Arc::new(FakePodman {
            ps: "[]".to_string(),
            seen,
        }),
        reqwest::Client::new(),
    ))
}

/// `runs_root` picks the tmpfs when `XDG_RUNTIME_DIR` is set and
/// `<data_dir>/agents/` when it is not — **per boot**. An install that lost the
/// variable for one boot wrote that boot's `secrets.json` to persistent
/// storage, and a sweep that only looked at the root in force now would leave
/// it there for good. Same for a `container_prefix` the owner has since
/// changed: its whole subtree is orphaned under the data dir.
#[tokio::test]
async fn the_sweep_collects_the_data_dir_fallback_and_a_former_prefix() {
    let (state, prefix) = gateway().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    state.set_runtime_for_tests(empty_ps(seen));

    // The fallback root for *this* prefix — what a boot without
    // XDG_RUNTIME_DIR would have written.
    let own_fallback = state
        .data_dir
        .join("agents")
        .join(lmgw_core::runtime::slug(&prefix));
    let a = container::RunDir::create(&own_fallback, 9).unwrap();
    let a_path = a.path().to_path_buf();
    std::mem::forget(a);

    // A prefix this install used to run under.
    let former = state.data_dir.join("agents").join("an-older-prefix");
    let b = container::RunDir::create(&former, 3).unwrap();
    let b_path = b.path().to_path_buf();
    std::mem::forget(b);

    // And a *sibling instance's* leaf on the shared tmpfs, which is not ours to
    // touch: `$XDG_RUNTIME_DIR/lmgw/` is shared by every lmgw on the box, which
    // is the whole reason `runs_root` scopes itself by prefix.
    let sibling = std::env::var("XDG_RUNTIME_DIR").ok().and_then(|x| {
        (!x.is_empty()).then(|| {
            std::path::PathBuf::from(x)
                .join("lmgw")
                .join(format!("{prefix}-sibling"))
        })
    });
    let sibling_dir = sibling.as_ref().map(|root| {
        let d = container::RunDir::create(root, 5).unwrap();
        let p = d.path().to_path_buf();
        std::mem::forget(d);
        p
    });

    for p in [&a_path, &b_path] {
        backdate(p);
    }
    if let Some(p) = &sibling_dir {
        backdate(p);
    }

    let report = container::reconcile_since(&state, after_everything()).await;
    assert!(
        !a_path.exists(),
        "the data-dir fallback was not swept: {report:?}"
    );
    assert!(
        !b_path.exists(),
        "a former prefix was not swept: {report:?}"
    );
    assert!(report.errors.is_empty(), "{report:?}");
    if let Some(p) = &sibling_dir {
        assert!(
            p.exists(),
            "another lmgw's run directory on the shared tmpfs must be left alone: {report:?}"
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}

/// Reconciliation is *spawned*, so the router is already serving while it runs:
/// by the time `podman ps` answers, the first App-tab request may have started
/// a service container and written its run directory. "Every agent container
/// whose `lmgw.run` is not a live job" would collect it seconds after it came
/// up.
#[tokio::test]
async fn reconciliation_leaves_alone_whatever_started_after_this_process_did() {
    let (state, prefix) = gateway().await;
    let name = format!("{prefix}-agentsvc-board");
    // `Created` in the future relative to the process start this reconcile is
    // given — i.e. a container that came up after the boot began.
    let ps = json!([
        { "Names": [name.clone()], "State": "running",
          "Created": chrono::Utc::now().timestamp() + 60,
          "Labels": { "lmgw.kind": "agent", "lmgw.instance": prefix,
                      "lmgw.run": "service", "lmgw.agent": "board" } },
    ])
    .to_string();
    let seen = Arc::new(Mutex::new(Vec::new()));
    state.set_runtime_for_tests(Arc::new(Registry::new(
        Arc::new(FakePodman {
            ps,
            seen: seen.clone(),
        }),
        reqwest::Client::new(),
    )));

    // Its run directory, created now — i.e. after this process started, which
    // is the state's own `started_at_utc`.
    let (root, _) = container::runs_root(&state.data_dir, &prefix);
    let dir = container::RunDir::create_named(&root, "service-board").unwrap();
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);

    let report = container::reconcile(&state).await;
    assert!(report.listed, "{report:?}");
    assert!(
        report.removed.is_empty(),
        "a container younger than this process is not a leftover: {report:?}"
    );
    assert!(
        report.swept_dirs.is_empty(),
        "nor is its run directory: {report:?}"
    );
    assert!(path.exists(), "the live service's secrets file was deleted");
    assert!(
        !seen.lock().unwrap().iter().any(|c| c[0] == "rm"),
        "{:?}",
        seen.lock().unwrap()
    );

    // Backdate the directory, and treat the container's own `Created` as past
    // — the ordinary rule applies again to both.
    backdate(&path);
    let later = chrono::Utc::now() + chrono::Duration::seconds(300);
    let report = container::reconcile_since(&state, later).await;
    assert_eq!(report.removed, vec![name], "{report:?}");
    assert!(
        report.swept_dirs.contains(&"service-board".to_string()),
        "{report:?}"
    );
    assert!(!path.exists());
}

/// Make a directory look like it predates this process — which is what a
/// leftover *is*. `filetime`-free: `File::set_times` on the directory's own
/// descriptor is `futimens`, which Linux allows on a directory opened
/// read-only.
fn backdate(path: &std::path::Path) {
    let when = std::time::SystemTime::now() - Duration::from_secs(3600);
    let f = std::fs::File::open(path).expect("open the directory");
    f.set_times(std::fs::FileTimes::new().set_modified(when))
        .expect("backdate the directory");
}

// ---------------------------------------------------------------------------
// The token never comes back out of the container's own output (§3.1)
// ---------------------------------------------------------------------------

/// A container that prints `/lmgw/secrets.json` — the one escape hatch §3.1
/// always named. What made it more than a documented hazard is where the text
/// goes afterwards: the run log is stored in the job's `result` and rendered on
/// the Run tab, which no credential should reach.
struct EchoesItsSecrets;

#[async_trait]
impl Spawner for EchoesItsSecrets {
    async fn spawn(&self, program: &str, args: &[String]) -> std::io::Result<Spawned> {
        assert_eq!(program, "podman");
        // Exactly what `cat /lmgw/secrets.json` would put on the two pipes.
        let secrets = mounted_files(args)
            .into_iter()
            .find(|(inside, _)| inside == "/lmgw/secrets.json")
            .map(|(_, body)| body)
            .expect("secrets.json is mounted");
        let token: String = serde_json::from_str::<Value>(&secrets).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        let (out_tx, stdout) = mpsc::channel(64);
        let (err_tx, stderr) = mpsc::channel(64);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            // Not JSON, so it lands in the run log verbatim — the worst case.
            let _ = out_tx.send(secrets.replace('\n', " ")).await;
            // And as a proper `log` event, which is the other way in.
            let _ = out_tx
                .send(json!({ "type": "log", "message": format!("using {token}") }).to_string())
                .await;
            let _ = out_tx
                .send(r#"{"type":"row","id":"m1","columns":{"subject":"Invoice"}}"#.to_string())
                .await;
            let _ = err_tx
                .send(format!("DEBUG Authorization: Bearer {token}"))
                .await;
            drop(out_tx);
            drop(err_tx);
            let _ = done_tx.send(Exit::Code(0));
        });
        Ok(Spawned {
            stdout,
            stderr,
            // The agent runner stops containers by name, never by signal.
            kill: container::KillHandle::detached(),
            status: Box::pin(async move { Ok(done_rx.await.unwrap_or(Exit::Code(-1))) }),
        })
    }

    async fn run(&self, _p: &str, _args: &[String]) -> std::io::Result<CmdOutput> {
        Ok(CmdOutput {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

#[tokio::test]
async fn a_container_that_prints_its_own_secrets_file_does_not_leak_the_token() {
    let (state, _prefix) = gateway().await;
    state.set_agent_spawner_for_tests(Arc::new(EchoesItsSecrets));
    let base = serve(state.clone()).await;
    install(&base, &doc("localhost/labeler:1", 600, r#"["run"]"#, "")).await;
    // Mint it first, so the test knows the exact value the run will be handed.
    let (_, got) = op(&base, "agent_token_get", json!({ "id": "labeler" })).await;
    let token = got["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("lmgw-agent-"));

    let d = wait_done(
        &base,
        start(&base, json!({ "id": "labeler", "phase": "run" })).await,
    )
    .await;
    assert_eq!(d["job"]["status"], "done", "{d}");

    // Three ways in — a raw stdout line, a `log` event and stderr — and none of
    // them came back out.
    let text = d.to_string();
    assert!(
        !text.contains(&token),
        "the agent's token is in the run detail: {text}"
    );
    let log = log(&d);
    assert_eq!(
        log.matches("<agent token>").count(),
        3,
        "each of the three lines has to be redacted, not dropped: {log}"
    );
    assert!(
        log.contains("DEBUG Authorization: Bearer <agent token>"),
        "{log}"
    );
    // The run itself is unaffected: redaction is not a failure mode.
    assert_eq!(rows(&d).len(), 1, "{d}");

    // And the stored `result` — what the Runs tab reads after the fact — is
    // clean too, because the redaction happens before the line is stored.
    let stored = store::get_job(&state.db, d["job"]["job_id"].as_i64().unwrap())
        .await
        .unwrap()
        .unwrap();
    let result = stored.result.unwrap_or_default();
    assert!(!result.contains(&token), "{result}");
}

/// A real `rw` mount, on real podman, with two containers on one folder
/// (mounts §5.5, §10 Part 3).
///
/// The two halves that cannot be asserted against a fake: what the kernel does
/// with `--userns=keep-id` — the file the container writes is the **owner's**,
/// readable straight off the host — and what `:z` does that `:Z` would not:
/// the second container starting against the same folder does not revoke the
/// first one's access to it. The writer proves that by reading the reader's
/// file *after* the reader has started and relabelled the same source.
///
/// The folder is a `$TMPDIR` one, never anything under `$HOME`: binding
/// relabels what it is given, recursively and permanently.
fn real_mount_doc(id: &str, image: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "{id}",
  "model": {{ "alias": "{{{{config.model}}}}" }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "model": {{ "type": "string", "format": "model_alias" }},
    "shared": {{ "type": "string", "format": "directory", "access": "rw" }}
  }} }} }},
  "run": {{
    "kind": "container",
    "image": "{image}",
    "columns": ["subject"],
    "phases": ["run"],
    "limits": {{ "memory_mb": 128, "cpus": 1.0, "pids": 64,
                "deadline_seconds": 120, "stop_grace_seconds": 1 }}
  }}
}}"#
    )
}

/// Writes its file, says so, then waits for the *other* container's file and
/// reads it — which is the `:z` assertion: with `:Z` the second start would
/// have taken this container's access to the folder away mid-run.
const REAL_WRITER_SCRIPT: &str = r#"#!/bin/sh
D=/lmgw/mounts/shared
echo "hello from a" > "$D/a.txt" || { echo "the rw mount is not writable: $?"; exit 1; }
echo "{\"type\":\"row\",\"id\":\"wrote\",\"columns\":{\"subject\":\"a.txt\"}}"
i=0
while [ $i -lt 600 ]; do
  [ -f "$D/b.txt" ] && break
  sleep 0.1
  i=$((i+1))
done
echo "{\"type\":\"row\",\"id\":\"read\",\"columns\":{\"subject\":\"$(cat "$D/b.txt" 2>&1)\"}}"
"#;

/// Reads the first container's file and leaves one of its own behind.
const REAL_READER_SCRIPT: &str = r#"#!/bin/sh
D=/lmgw/mounts/shared
echo "{\"type\":\"row\",\"id\":\"read\",\"columns\":{\"subject\":\"$(cat "$D/a.txt" 2>&1)\"}}"
echo "hello from b" > "$D/b.txt" || { echo "the rw mount is not writable: $?"; exit 1; }
"#;

#[tokio::test]
async fn the_real_thing_writes_through_an_rw_mount_and_two_containers_share_it() {
    if !podman_available() {
        eprintln!("SKIP the_real_thing_writes…: podman is not available on this box");
        return;
    }
    if !image_present(BASE_IMAGE) {
        eprintln!("SKIP the_real_thing_writes…: {BASE_IMAGE} is not on this box");
        return;
    }
    let (state, prefix) = gateway().await;
    let _sweep = ContainerSweep(prefix.clone());
    let pid = std::process::id();
    let writer = BuiltImage::new(&format!("mountw{pid}"), REAL_WRITER_SCRIPT);
    let reader = BuiltImage::new(&format!("mountr{pid}"), REAL_READER_SCRIPT);
    state.set_agent_spawner_for_tests(Arc::new(container::TokioSpawner));
    let base = serve(state.clone()).await;

    let folder = tempfile::tempdir().expect("a temp dir");
    let shared = std::fs::canonicalize(folder.path()).unwrap();
    let shared_str = shared.display().to_string();
    install(&base, &real_mount_doc("notes-a", writer.name())).await;
    install(&base, &real_mount_doc("notes-b", reader.name())).await;
    for id in ["notes-a", "notes-b"] {
        let (status, v) = op(
            &base,
            "agent_config_set",
            json!({ "id": id, "values": { "shared": shared_str } }),
        )
        .await;
        assert_eq!(status, 200, "binding {id}: {v}");
    }

    // The writer first, and only once its file is really there does the reader
    // start — so the second container is provably starting against a folder the
    // first is still holding.
    let a = start(&base, json!({ "id": "notes-a", "phase": "run" })).await;
    let written = shared.join("a.txt");
    for _ in 0..1500 {
        if written.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        written.exists(),
        "the container never wrote through the mount; run log: {}",
        log(&get_json(&base, &format!("/api/agents/runs/{a}")).await)
    );

    // What keep-id is for: the file is the owner's, readable off the host with
    // no unpicking of a subuid.
    let body = std::fs::read_to_string(&written).expect("the host reads what the container wrote");
    assert_eq!(body.trim(), "hello from a");
    let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&written).unwrap());
    let mine = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata("/proc/self").unwrap());
    assert_eq!(uid, mine, "the container wrote as somebody else");

    let b = start(&base, json!({ "id": "notes-b", "phase": "run" })).await;
    let db = wait_done(&base, b).await;
    assert_eq!(db["job"]["status"], "done", "{db}");
    assert_eq!(
        rows(&db)[0]["columns"]["subject"],
        json!("hello from a"),
        "the second container read the first one's file: {db}"
    );

    let da = wait_done(&base, a).await;
    assert_eq!(da["job"]["status"], "done", "{da}");
    let first = rows(&da);
    assert_eq!(
        first
            .iter()
            .find(|r| r["id"] == json!("read"))
            .map(|r| r["columns"]["subject"].clone()),
        Some(json!("hello from b")),
        "`:z` is what keeps the first container's access when the second \
         starts on the same folder: {da}"
    );
    // And the run log said what it was doing to the folder, once per mount.
    assert!(
        log(&da).contains(&format!(
            "mount shared: {shared_str} → /lmgw/mounts/shared (rw, \
                                    directory; relabelled container_file_t, recursive, \
                                    permanent)"
        )),
        "{}",
        log(&da)
    );

    // The relabel really happened, when there is a tool on this box to ask.
    if let Ok(out) = std::process::Command::new("ls")
        .args(["-Zd", &shared_str])
        .output()
    {
        let seen = String::from_utf8_lossy(&out.stdout).to_string();
        if seen.contains(':') {
            assert!(
                seen.contains("container_file_t"),
                "the source was not relabelled: {seen}"
            );
        }
    }
}
