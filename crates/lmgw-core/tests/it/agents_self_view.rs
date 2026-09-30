//! What a container sees of itself (principals design §3.10, §10 Part 1).
//!
//! `GET /api/agents/{id}` answers two readers out of one document — the owner's
//! page, and the container reading its own row with its own token — and this
//! suite is the difference between the two renderings. Today that is `dev_url`,
//! which is a port on the owner's desk; WP3 adds the mount substitution in the
//! same place.
//!
//! Two things that are **not** a view difference are here for the same reason:
//! the masking a degraded row used to skip, which was a hole for every
//! principal rather than a leak to one of them, and the advisory that says out
//! loud what an agent token does not confine while *Require API key* is off.
//!
//! The runs routes are `AgentSelf` too, and almost none of a run splits by
//! view: its rows, its result and its batch shape are that agent's own output.
//! Two fields do — `log` and `error`, the two that carry a **host path**,
//! because the run log prints every bound mount for the owner (principle 4)
//! and a use-time refusal names the folder it refused.

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lmgw_core::agents::container::{self, Exit, Spawned, Spawner};
use lmgw_core::runtime::registry::CmdOutput;
use lmgw_core::state::AppState;
use lmgw_core::store;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use common::{serve, Gw};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The advisory's sentence, verbatim from §3.10. The Definition tab prints what
/// the server hands it, so the whole string is asserted rather than a fragment:
/// this is the one copy of it that is not in the server.
const ADVISORY: &str = "Require API key is off, so this token's model scope, tool allow-list and \
                        budget bind only a container that presents it; switch it on under \
                        Settings → Network & access to make them binding";

/// A container agent with a service and one secret field: the shape the
/// advisory is about, and the one whose config has something worth masking.
fn container_doc(id: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "Board",
  "description": "an agent that serves its own page",
  "model": {{ "alias": "m1" }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "api_key": {{ "type": "string", "format": "secret" }},
    "label": {{ "type": "string" }}
  }} }} }},
  "run": {{ "kind": "container", "image": "localhost/board:1",
    "limits": {{ "stop_grace_seconds": 1 }},
    "service": {{ "port": 8080 }} }}
}}"#
    )
}

/// The same container agent with one `rw` directory slot (mounts §5.1): the
/// second difference between the two views, and the one §3.10 was written for.
fn mount_doc(id: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "Board",
  "model": {{ "alias": "m1" }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "notes": {{ "type": "string", "format": "directory", "access": "rw" }},
    "label": {{ "type": "string" }}
  }} }} }},
  "run": {{ "kind": "container", "image": "localhost/board:1",
    "limits": {{ "stop_grace_seconds": 1 }},
    "service": {{ "port": 8080 }} }}
}}"#
    )
}

/// The same slot on a container agent with a `run` phase and no service: what
/// a phase run binds, so there is a run log with a mount line in it and a
/// start that can be refused by the path rules.
fn run_mount_doc(id: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "Notes desk",
  "model": {{ "alias": "m1" }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "notes": {{ "type": "string", "format": "directory", "access": "rw" }}
  }} }} }},
  "run": {{ "kind": "container", "image": "localhost/notes:1",
    "phases": ["run"],
    "limits": {{ "deadline_seconds": 600, "stop_grace_seconds": 1 }} }}
}}"#
    )
}

/// A podman that is there and a container that prints one row and exits 0.
///
/// Small on purpose: the suite next door owns the runtime's behaviour, and
/// what is under test here is the *document* two principals read afterwards.
struct Fake;

#[async_trait]
impl Spawner for Fake {
    async fn spawn(&self, _program: &str, _args: &[String]) -> std::io::Result<Spawned> {
        let (out_tx, stdout) = mpsc::channel(8);
        let (_err_tx, stderr) = mpsc::channel(8);
        tokio::spawn(async move {
            let _ = out_tx
                .send(r#"{"type":"row","id":"m1","columns":{"subject":"Invoice"}}"#.to_string())
                .await;
        });
        Ok(Spawned {
            stdout,
            stderr,
            // The agent runner stops containers by name, never by signal.
            kill: container::KillHandle::detached(),
            status: Box::pin(async move { Ok(Exit::Code(0)) }),
        })
    }
    async fn run(&self, _program: &str, _args: &[String]) -> std::io::Result<CmdOutput> {
        Ok(CmdOutput {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// A batch agent: its phases are this desk's, so there is no image out there
/// holding a token and no advisory, whatever the toggle says.
fn batch_doc(id: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "Labeler",
  "model": {{ "alias": "m1" }},
  "run": {{ "kind": "batch",
    "source": {{ "tool": "gws__search" }},
    "item": {{ "id": "{{{{item.id}}}}", "columns": {{ "subject": "{{{{item.subject}}}}" }} }},
    "limits": {{ "deadline_seconds": 0 }} }}
}}"#
    )
}

async fn op(gw: &Gw, name: &str, args: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let v = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("op {name} is not JSON ({e}): {text}"));
    (status, v)
}

async fn install(gw: &Gw, manifest: &str) {
    let (status, body) = op(
        gw,
        "agent_set",
        json!({ "manifest": manifest, "replace": true }),
    )
    .await;
    assert_eq!(status, 200, "agent_set: {body}");
}

/// The agent's own token, minted through the op the owner would press.
async fn token_for(gw: &Gw, id: &str) -> String {
    let (status, body) = op(gw, "agent_token_get", json!({ "id": id })).await;
    assert_eq!(status, 200, "agent_token_get: {body}");
    body["token"].as_str().expect("a minted token").to_string()
}

/// A read as the owner — the dashboard session, which is what `Gw::client`
/// carries.
async fn as_owner(gw: &Gw, path: &str) -> (u16, Value) {
    let resp = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// The same read as the container: one bearer, no session.
async fn as_agent(gw: &Gw, path: &str, token: &str) -> (u16, Value) {
    let resp = gw
        .anon()
        .get(format!("{gw}{path}"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn post_as(gw: &Gw, path: &str, token: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .anon()
        .post(format!("{gw}{path}"))
        .header("authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn warning<'a>(d: &'a Value, code: &str) -> Option<&'a Value> {
    d["warnings"]
        .as_array()?
        .iter()
        .find(|w| w["code"] == json!(code))
}

/// Wait until `job` has left `running`, so both readers are compared against a
/// document that has stopped moving.
async fn settled(gw: &Gw, job: i64) {
    for _ in 0..200 {
        let (_, v) = as_owner(gw, &format!("/api/agents/runs/{job}")).await;
        let status = v["job"]["status"].as_str().unwrap_or_default().to_string();
        if status != "running" && status != "queued" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("run {job} never settled");
}

// ---------------------------------------------------------------------------
// §3.10 — the agent view
// ---------------------------------------------------------------------------

/// The one difference this work package makes to the document: a `dev_url` is
/// the owner's own port, and the container has no business knowing it is there.
/// Everything else — the manifest, the masked config — reads the same.
#[tokio::test]
async fn the_agent_view_blanks_dev_url_and_the_owners_keeps_it() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    install(&gw, &container_doc("board")).await;
    store::set_agent_config(
        &state.db,
        "board",
        &json!({ "api_key": "sk-the-real-secret", "label": "hello" }).to_string(),
    )
    .await
    .unwrap();
    store::set_agent_dev_url(&state.db, "board", Some("http://127.0.0.1:5173"))
        .await
        .unwrap();
    let token = token_for(&gw, "board").await;

    let (status, owner) = as_owner(&gw, "/api/agents/board").await;
    assert_eq!(status, 200, "{owner}");
    assert_eq!(owner["dev_url"], json!("http://127.0.0.1:5173"));
    assert_eq!(
        owner["config"]["api_key"],
        json!({ "has_value": true }),
        "a secret never leaves, in either view: {owner}"
    );

    let (status, mine) = as_agent(&gw, "/api/agents/board", &token).await;
    assert_eq!(status, 200, "{mine}");
    assert_eq!(mine["id"], json!("board"));
    assert_eq!(
        mine["dev_url"],
        json!(""),
        "a dev_url is a port on the owner's desk: {mine}"
    );
    assert_eq!(mine["config"]["api_key"], json!({ "has_value": true }));
    assert_eq!(
        mine["config"]["label"],
        json!("hello"),
        "the agent still reads its own configuration: {mine}"
    );
    assert!(
        !mine.to_string().contains("sk-the-real-secret"),
        "the plaintext is nowhere in the document: {mine}"
    );
    assert_eq!(
        mine["manifest"], owner["manifest"],
        "one document, two renderings — the manifest is not one of the differences"
    );
}

/// The mount substitution, in both halves of the document (mounts §5.2,
/// principals §3.10): the owner reads the folder they picked, the container
/// reads where that folder is from inside, and `service.mounts[]` carries the
/// host path for one of them and not the other.
#[tokio::test]
async fn the_agent_view_reads_a_mount_as_its_container_path_and_the_owner_reads_the_folder() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let notes = std::fs::canonicalize(tmp.path()).unwrap();
    let notes = notes.display().to_string();
    install(&gw, &mount_doc("board")).await;
    let (status, v) = op(
        &gw,
        "agent_config_set",
        json!({ "id": "board", "values": { "notes": notes, "label": "hello" } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let token = token_for(&gw, "board").await;

    let (status, owner) = as_owner(&gw, "/api/agents/board").await;
    assert_eq!(status, 200, "{owner}");
    assert_eq!(
        owner["config"]["notes"],
        json!(notes),
        "the owner sees the folder they picked: {owner}"
    );
    assert_eq!(
        owner["service"]["mounts"],
        json!([{ "field": "notes", "host": notes, "inside": "/lmgw/mounts/notes",
                 "kind": "directory", "access": "rw" }]),
        "{owner}"
    );

    let (status, mine) = as_agent(&gw, "/api/agents/board", &token).await;
    assert_eq!(status, 200, "{mine}");
    assert_eq!(
        mine["config"]["notes"],
        json!("/lmgw/mounts/notes"),
        "a manifest names a slot and the owner names the folder: {mine}"
    );
    assert_eq!(
        mine["config"]["label"],
        json!("hello"),
        "everything that is not a path still reads the same: {mine}"
    );
    assert_eq!(
        mine["service"]["mounts"],
        json!([{ "field": "notes", "inside": "/lmgw/mounts/notes",
                 "kind": "directory", "access": "rw" }]),
        "no host, and absent rather than empty: {mine}"
    );
    assert!(
        !mine.to_string().contains(&notes),
        "the host path reached the container's own view: {mine}"
    );
}

/// The same for a row this build cannot parse: the raw manifest still says
/// which property is a slot, so the agent's copy still reads the container
/// path — the degraded document is not a way around §3.10.
#[tokio::test]
async fn a_degraded_row_still_hides_the_host_path_from_the_agent() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let notes = std::fs::canonicalize(tmp.path()).unwrap();
    let notes = notes.display().to_string();
    install(&gw, &mount_doc("board")).await;
    let (status, v) = op(
        &gw,
        "agent_config_set",
        json!({ "id": "board", "values": { "notes": notes } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let token = token_for(&gw, "board").await;
    let from_the_future = mount_doc("board").replace("\"container\"", "\"swarm\"");
    store::update_agent_manifest(&state.db, "board", &from_the_future)
        .await
        .unwrap();

    let (status, mine) = as_agent(&gw, "/api/agents/board", &token).await;
    assert_eq!(status, 200, "{mine}");
    assert_eq!(
        mine["config"]["notes"],
        json!("/lmgw/mounts/notes"),
        "{mine}"
    );
    assert!(!mine.to_string().contains(&notes), "{mine}");
    let (_, owner) = as_owner(&gw, "/api/agents/board").await;
    assert_eq!(
        owner["config"]["notes"],
        json!(notes),
        "and the owner still reads their own path off the row: {owner}"
    );
}

/// A row this build cannot parse used to hand its stored config back **in the
/// clear**, on the reasoning that there was no schema left to mask against
/// (§3.10). There is: the raw JSON still says which property is a secret.
#[tokio::test]
async fn a_row_this_build_cannot_read_is_masked_for_both_readers() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    install(&gw, &container_doc("board")).await;
    store::set_agent_config(
        &state.db,
        "board",
        &json!({ "api_key": "sk-the-real-secret", "label": "hello" }).to_string(),
    )
    .await
    .unwrap();
    let token = token_for(&gw, "board").await;

    // A `run.kind` this build has never heard of, over the config the row
    // already holds: replacing a manifest keeps the config (§5), which is
    // exactly how a degraded row comes to be holding a secret.
    let from_the_future = container_doc("board").replace("\"container\"", "\"swarm\"");
    store::update_agent_manifest(&state.db, "board", &from_the_future)
        .await
        .unwrap();

    for (who, (status, d)) in [
        ("the owner", as_owner(&gw, "/api/agents/board").await),
        (
            "the agent itself",
            as_agent(&gw, "/api/agents/board", &token).await,
        ),
    ] {
        assert_eq!(status, 200, "a degraded row must not 400 for {who}: {d}");
        assert!(
            d["error"].as_str().unwrap_or_default().contains("swarm"),
            "{who} is told why: {d}"
        );
        assert!(warning(&d, "manifest_unreadable").is_some(), "{who}: {d}");
        assert_eq!(
            d["config"]["api_key"],
            json!({ "has_value": true }),
            "the secret is masked for {who} too: {d}"
        );
        assert_eq!(
            d["config"]["label"],
            json!("hello"),
            "what is not a secret is still readable by {who}: {d}"
        );
        assert!(
            !d.to_string().contains("sk-the-real-secret"),
            "the plaintext reached {who}: {d}"
        );
    }

    // And the floor under all of it: a manifest that is not even JSON leaves
    // nothing to read a schema out of, so every value is masked. "Could not
    // tell" has to read as "secret".
    store::insert_agent(&state.db, "rubble", "not json at all {", "imported")
        .await
        .unwrap();
    store::set_agent_config(
        &state.db,
        "rubble",
        &json!({ "api_key": "sk-the-real-secret", "label": "hello" }).to_string(),
    )
    .await
    .unwrap();
    let (status, d) = as_owner(&gw, "/api/agents/rubble").await;
    assert_eq!(status, 200, "{d}");
    assert_eq!(d["config"]["api_key"], json!({ "has_value": true }), "{d}");
    assert_eq!(
        d["config"]["label"],
        json!({ "has_value": true }),
        "nothing says this one is safe to print: {d}"
    );
}

/// `token_scope_advisory` is present exactly when both halves are: the toggle
/// is off, and there is a container out there that would have to present the
/// token for its scope to mean anything (§3.10).
#[tokio::test]
async fn the_advisory_is_present_iff_auth_is_off_and_the_agent_is_a_container() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    install(&gw, &container_doc("board")).await;
    install(&gw, &batch_doc("labeler")).await;

    // Off — the shipped default.
    let (_, board) = as_owner(&gw, "/api/agents/board").await;
    let advisory = warning(&board, "token_scope_advisory")
        .unwrap_or_else(|| panic!("no advisory on a container agent: {board}"));
    assert_eq!(
        advisory["message"],
        json!(ADVISORY),
        "the sentence is the server's, rendered verbatim"
    );
    assert_eq!(
        advisory["blocks_start"],
        json!(false),
        "non-blocking: the fix is a checkbox, not an edit to this agent"
    );
    let (_, labeler) = as_owner(&gw, "/api/agents/labeler").await;
    assert!(
        warning(&labeler, "token_scope_advisory").is_none(),
        "a batch agent's phases are this desk's: {labeler}"
    );

    // On: the token's scope binds, so there is nothing to say.
    let (status, body) = op(&gw, "settings_set", json!({ "auth_enabled": true })).await;
    assert_eq!(status, 200, "{body}");
    for id in ["board", "labeler"] {
        let (_, d) = as_owner(&gw, &format!("/api/agents/{id}")).await;
        assert!(
            warning(&d, "token_scope_advisory").is_none(),
            "{id} with the toggle on: {d}"
        );
    }
}

// ---------------------------------------------------------------------------
// §3.2 — the runs routes, read as the container sees them
// ---------------------------------------------------------------------------

/// There is nothing in a run to take out of the container's copy: the rows, the
/// result and the log are that run's own output, the batch shape is its own
/// manifest's, and the `(kind, key)` filter is what keeps another agent's runs
/// out. So both principals read the same bytes — asserted, rather than assumed.
#[tokio::test]
async fn the_runs_routes_read_the_same_for_the_agent_and_for_the_owner() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    install(&gw, &batch_doc("labeler")).await;
    install(&gw, &batch_doc("other")).await;
    let mine = token_for(&gw, "labeler").await;
    let theirs = token_for(&gw, "other").await;

    let (status, opened) = post_as(
        &gw,
        "/api/agents/labeler/runs",
        &mine,
        json!({ "phase": "run" }),
    )
    .await;
    assert_eq!(status, 200, "{opened}");
    let run = opened["run"].as_i64().unwrap();
    post_as(
        &gw,
        &format!("/api/agents/runs/{run}/events"),
        &mine,
        json!([{ "type": "row", "id": "m1", "columns": { "subject": "Invoice" } }]),
    )
    .await;
    post_as(
        &gw,
        &format!("/api/agents/runs/{run}/close"),
        &mine,
        json!({ "status": "done", "output": { "applied": 1 } }),
    )
    .await;
    settled(&gw, run).await;

    // The other agent opens one of its own, so "every run of this agent" has
    // something to leave out.
    let (_, elsewhere) = post_as(
        &gw,
        "/api/agents/other/runs",
        &theirs,
        json!({ "phase": "run" }),
    )
    .await;
    let foreign = elsewhere["run"].as_i64().unwrap();

    let (status, list_agent) = as_agent(&gw, "/api/agents/labeler/runs", &mine).await;
    assert_eq!(status, 200, "{list_agent}");
    let (_, list_owner) = as_owner(&gw, "/api/agents/labeler/runs").await;
    assert_eq!(
        list_agent, list_owner,
        "the runs list has no view split: {list_agent}"
    );
    let ids: Vec<i64> = list_agent
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["job_id"].as_i64())
        .collect();
    assert_eq!(ids, vec![run], "only its own runs: {list_agent}");

    let (status, detail_agent) = as_agent(&gw, &format!("/api/agents/runs/{run}"), &mine).await;
    assert_eq!(status, 200, "{detail_agent}");
    let (_, detail_owner) = as_owner(&gw, &format!("/api/agents/runs/{run}")).await;
    assert_eq!(
        detail_agent, detail_owner,
        "and neither does one run: {detail_agent}"
    );
    assert_eq!(
        detail_agent["rows"][0]["columns"]["subject"],
        json!("Invoice")
    );

    // And the other agent's run is still not readable, which is the ownership
    // check rather than the view.
    let (status, refused) = as_agent(&gw, &format!("/api/agents/runs/{foreign}"), &mine).await;
    assert_eq!(status, 403, "{refused}");
}

/// A start refused by the path rules fails the job with the refusal's own
/// sentence — the host path included — and `jobs.error` comes back out of both
/// runs routes, one of which the agent reads with its own token (mounts §5.3,
/// principals §3.10).
///
/// So the agent gets the field and the code and nothing else. The owner, who
/// has to go and find the folder, gets the whole thing.
#[tokio::test]
async fn a_start_refused_by_a_mount_rule_reads_as_the_code_alone_to_the_agent() {
    let state = AppState::init_for_tests().await.unwrap();
    state.set_agent_spawner_for_tests(Arc::new(Fake));
    let gw = serve(state.clone()).await;
    install(&gw, &run_mount_doc("notes-desk")).await;
    let tmp = tempfile::tempdir().unwrap();
    let notes = std::fs::canonicalize(tmp.path())
        .unwrap()
        .display()
        .to_string();
    let (status, v) = op(
        &gw,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": notes } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let token = token_for(&gw, "notes-desk").await;

    // The folder goes between the save and the start, which is the whole
    // reason the rules are checked twice.
    std::fs::remove_dir_all(tmp.path()).unwrap();
    let (status, v) = op(
        &gw,
        "agent_run",
        json!({ "id": "notes-desk", "phase": "run" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let job = v["job_id"].as_i64().unwrap();
    settled(&gw, job).await;

    for path in [
        format!("/api/agents/runs/{job}"),
        "/api/agents/notes-desk/runs".to_string(),
    ] {
        let (status, owner) = as_owner(&gw, &path).await;
        assert_eq!(status, 200, "{owner}");
        let theirs = owner.to_string();
        assert!(theirs.contains("mount_path_missing"), "{theirs}");
        assert!(
            theirs.contains(&notes),
            "the owner has to be told which folder: {theirs}"
        );

        let (status, mine) = as_agent(&gw, &path, &token).await;
        assert_eq!(status, 200, "{mine}");
        let error = match mine.get("job") {
            Some(job) => job["error"].clone(),
            None => mine[0]["error"].clone(),
        };
        assert_eq!(
            error,
            json!("notes: mount_path_missing"),
            "the field and the code, and no path from anyone: {mine}"
        );
        assert!(
            !mine.to_string().contains(&notes),
            "the host path reached the container: {mine}"
        );
    }
}

/// The run log's per-mount line is host path to container path, because the
/// owner chose the folder and has to be able to see what was bound (principle
/// 4). The container did not choose it and has no use for it: its copy of the
/// same line reads in the container's own coordinates (mounts §5.6).
#[tokio::test]
async fn the_run_logs_mount_line_reads_in_container_paths_for_the_agent() {
    let state = AppState::init_for_tests().await.unwrap();
    state.set_agent_spawner_for_tests(Arc::new(Fake));
    let gw = serve(state.clone()).await;
    install(&gw, &run_mount_doc("notes-desk")).await;
    let tmp = tempfile::tempdir().unwrap();
    let notes = std::fs::canonicalize(tmp.path())
        .unwrap()
        .display()
        .to_string();
    let (status, v) = op(
        &gw,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": notes } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let token = token_for(&gw, "notes-desk").await;

    let (status, v) = op(
        &gw,
        "agent_run",
        json!({ "id": "notes-desk", "phase": "run" }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let job = v["job_id"].as_i64().unwrap();
    settled(&gw, job).await;

    let (_, owner) = as_owner(&gw, &format!("/api/agents/runs/{job}")).await;
    assert_eq!(owner["job"]["status"], json!("done"), "{owner}");
    let theirs = owner["log"].to_string();
    assert!(
        theirs.contains(&format!("mount notes: {notes} → /lmgw/mounts/notes")),
        "{theirs}"
    );

    let (_, mine) = as_agent(&gw, &format!("/api/agents/runs/{job}"), &token).await;
    let ours = mine["log"].to_string();
    assert!(
        ours.contains("mount notes: /lmgw/mounts/notes → /lmgw/mounts/notes"),
        "the line survives, in the container's own coordinates: {ours}"
    );
    assert!(
        ours.contains("relabelled container_file_t"),
        "and says what binding did: {ours}"
    );
    assert!(!mine.to_string().contains(&notes), "{mine}");
}

/// An empty value is an **unbound** slot, and an unbound slot is absent — the
/// rule `input.json` already follows (mounts §5.6).
///
/// The container's view used to substitute on the key alone, so a slot the
/// owner had cleared read as `/lmgw/mounts/notes` while nothing was mounted
/// there: a container told to write into a path that is not a mount point
/// writes into its own filesystem and loses it at the next start.
#[tokio::test]
async fn a_cleared_mount_is_absent_from_the_agents_view_rather_than_a_path() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let notes = std::fs::canonicalize(tmp.path()).unwrap();
    let notes = notes.display().to_string();
    install(&gw, &mount_doc("board")).await;
    let (status, v) = op(
        &gw,
        "agent_config_set",
        json!({ "id": "board", "values": { "notes": notes, "label": "hello" } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let token = token_for(&gw, "board").await;
    let (_, mine) = as_agent(&gw, "/api/agents/board", &token).await;
    assert_eq!(
        mine["config"]["notes"],
        json!("/lmgw/mounts/notes"),
        "{mine}"
    );

    // Cleared on the Run tab: the key stays, the value is empty.
    let (status, v) = op(
        &gw,
        "agent_config_set",
        json!({ "id": "board", "values": { "notes": "" } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["config"]["notes"], json!(""), "{v}");

    let (status, mine) = as_agent(&gw, "/api/agents/board", &token).await;
    assert_eq!(status, 200, "{mine}");
    assert!(
        mine["config"].get("notes").is_none(),
        "nothing is mounted there, so there is no path to report: {mine}"
    );
    assert_eq!(mine["config"]["label"], json!("hello"), "{mine}");
    // And `service.mounts[]` agrees: an unbound slot is not a mount.
    assert_eq!(mine["service"]["mounts"], json!([]), "{mine}");
}

/// The read is not an enumeration oracle (principals §10 Part 1): to an agent,
/// a run that is not its own and a run that is nobody's are the **same**
/// refusal, down to the bytes.
///
/// Answering `404` for the second would turn the difference between the two
/// into a directory of every other agent's job ids, walkable one small integer
/// at a time. The owner reads every run, so for the owner a missing id is
/// still a missing id.
#[tokio::test]
async fn a_missing_run_and_another_agents_run_read_alike_to_an_agent() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    install(&gw, &batch_doc("labeler")).await;
    install(&gw, &batch_doc("other")).await;
    let mine = token_for(&gw, "labeler").await;
    let theirs = token_for(&gw, "other").await;

    let (status, opened) = post_as(
        &gw,
        "/api/agents/other/runs",
        &theirs,
        json!({ "phase": "run" }),
    )
    .await;
    assert_eq!(status, 200, "{opened}");
    let foreign = opened["run"].as_i64().unwrap();
    let nobodys = foreign + 10_000;

    let (their_status, their_body) =
        as_agent(&gw, &format!("/api/agents/runs/{foreign}"), &mine).await;
    let (no_status, no_body) = as_agent(&gw, &format!("/api/agents/runs/{nobodys}"), &mine).await;
    assert_eq!(their_status, 403, "{their_body}");
    assert_eq!(their_body["code"], json!("run_not_owned"), "{their_body}");
    assert_eq!(
        (no_status, no_body),
        (their_status, their_body),
        "a run id nothing owns must read exactly as one another agent owns"
    );

    // The owner is not being told a story: a missing id is missing.
    let (status, body) = as_owner(&gw, &format!("/api/agents/runs/{nobodys}")).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["code"], json!("not_found"), "{body}");
    // And a run that exists is still readable by whoever may read it.
    let (status, body) = as_owner(&gw, &format!("/api/agents/runs/{foreign}")).await;
    assert_eq!(status, 200, "{body}");
}
