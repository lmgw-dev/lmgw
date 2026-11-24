//! Async conversion jobs over HTTP (document-conversion design §7.3, §7.6,
//! WP5): a job per request, its state and result, one file's entry while
//! the rest wait, cancel (tools stopped, slots freed, results deleted), a
//! job in flight across a restart (`interrupted`), and the retention.

use base64::Engine as _;
use lmgw_core::extract::convert::jobs::{results, sweep};
use lmgw_core::extract::convert::pool;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::chat_attach_kinds::docx;
use crate::common::{serve, Gw};

async fn gateway() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn get(gw: &Gw, path: &str) -> (u16, Value) {
    let r = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
    let s = r.status().as_u16();
    (s, r.json().await.unwrap_or(Value::Null))
}

async fn start(gw: &Gw, files: Value) -> String {
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(&json!({"files": files, "async": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 202);
    let loc = r.headers()["location"].to_str().unwrap().to_string();
    let v: Value = r.json().await.unwrap();
    let id = v["job"].as_str().unwrap().to_string();
    assert_eq!(loc, format!("/v1/documents/jobs/{id}"));
    id
}

/// Once a step of the job waits for the pool's slot (bounded: the slot is
/// held by the test, and a job's task reaches it at once unless the
/// machine is loaded). The job's task runs on its own: at 32 test threads
/// beside a CPU hog (2026-11-20) the first GET, or the test's stand-in
/// restart, came before the task had read its files' uploads and queued.
async fn queued(id: &str) {
    let waited = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while pool::global().facts().waiting == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "job {id}'s steps never waited for the slot");
}

/// The job's state once it has ended (bounded: a job here converts text).
async fn ended(state: &SharedState, gw: &Gw, id: &str) -> Value {
    if let Some(mut rx) = state.convert_jobs.watch(id) {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while rx.changed().await.is_ok() {
                if state.convert_jobs.watch(id).is_none() {
                    break;
                }
            }
        })
        .await;
    }
    get(gw, &format!("/v1/documents/jobs/{id}")).await.1
}

#[tokio::test]
async fn an_async_request_answers_a_job_whose_result_is_the_sync_answer() {
    let (state, gw) = gateway().await;
    let files = json!([
        {"name": "example-a.txt", "data_base64": b64(b"# Notes\n\nplain text")},
        {"name": "example-b.docx", "data_base64": b64(&docx("Hello from Word"))},
        {"name": "example-c.wav", "data_base64": b64(b"RIFF\0\0\0\0WAVE")},
    ]);
    let id = start(&gw, files.clone()).await;
    let job = ended(&state, &gw, &id).await;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["files"], 3);
    assert_eq!(job["documents"][1]["state"], "converted");
    assert_eq!(job["documents"][2]["state"], "failed");
    // No speech-to-text model is chosen (WP7).
    assert_eq!(job["documents"][2]["error"], "model_not_chosen");
    assert_eq!(job["parts_done"], job["parts"]);
    assert!(job["pool"].is_null(), "an ended job has no pool");

    let (s, a) = get(&gw, &format!("/v1/documents/jobs/{id}/result")).await;
    assert_eq!(s, 200, "{a}");
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(&json!({"files": files}))
        .send()
        .await
        .unwrap();
    let mut sync: Value = r.json().await.unwrap();
    let mut a = a;
    for v in [&mut sync, &mut a] {
        for d in v["documents"].as_array_mut().unwrap() {
            d["stats"]["time_ms"] = json!(0);
        }
    }
    assert_eq!(a, sync, "the job's result is the sync answer");
    // One file's entry, and the native route's results are not single use.
    let (s, e) = get(&gw, &format!("/v1/documents/jobs/{id}/result?index=1")).await;
    assert_eq!(s, 200);
    assert_eq!(e["content"], "Hello from Word");
    let (s, _) = get(&gw, &format!("/v1/documents/jobs/{id}/result")).await;
    assert_eq!(s, 200);
    // Stored where the design says, as the entry's JSON.
    let path = state
        .data_dir
        .join("convert")
        .join(&id)
        .join("1")
        .join("entry.json");
    assert!(path.exists(), "{path:?}");
}

#[tokio::test]
async fn another_key_gets_a_404_and_the_owner_sees_every_job() {
    let (state, gw) = gateway().await;
    let mut settings = state.snapshot().settings.clone();
    settings.auth_enabled = true;
    store::save_settings(&state.db, &settings).await.unwrap();
    for (name, key) in [
        ("example-a", "lmgw-example-a"),
        ("example-b", "lmgw-example-b"),
    ] {
        store::insert_api_key(&state.db, name, &lmgw_core::config::hash_api_key(key))
            .await
            .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let with = |key: &str| {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("x-api-key", key.parse().unwrap());
        reqwest::Client::builder()
            .default_headers(h)
            .build()
            .unwrap()
    };
    let r = with("lmgw-example-a")
        .post(format!("{gw}/v1/documents/convert"))
        .json(&json!({"files": [{"name": "a.txt", "data_base64": b64(b"x")}], "async": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 202);
    let id = r.json::<Value>().await.unwrap()["job"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("{gw}/v1/documents/jobs/{id}");
    let other = with("lmgw-example-b").get(&path).send().await.unwrap();
    assert_eq!(other.status().as_u16(), 404);
    let own = with("lmgw-example-a").get(&path).send().await.unwrap();
    assert_eq!(own.status().as_u16(), 200);
    assert_eq!(
        gw.client()
            .get(&path)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
}

#[tokio::test]
async fn a_cancelled_job_frees_its_slots_and_its_results() {
    let (state, gw) = gateway().await;
    // One slot, held here: the job's files wait for it.
    pool::configure(1);
    let held = pool::global().acquire().await;
    let id = start(
        &gw,
        json!([
            {"name": "a.txt", "data_base64": b64(b"one")},
            {"name": "b.docx", "data_base64": b64(&docx("two"))},
        ]),
    )
    .await;
    queued(&id).await;
    let (_, job) = get(&gw, &format!("/v1/documents/jobs/{id}")).await;
    assert_eq!(job["state"], "running");
    assert!(job["pool"]["waiting"].as_u64().unwrap() >= 1, "{job}");
    let (s, e) = get(&gw, &format!("/v1/documents/jobs/{id}/result")).await;
    assert_eq!((s, e["error"]["code"].as_str()), (409, Some("job_running")));
    let r = gw
        .client()
        .delete(format!("{gw}/v1/documents/jobs/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let job: Value = r.json().await.unwrap();
    assert_eq!(job["state"], "cancelled");
    assert!(job["documents"]
        .as_array()
        .unwrap()
        .iter()
        .all(|d| d["state"] == "cancelled"));
    assert_eq!(
        pool::global().facts().waiting,
        0,
        "the waiting steps are gone"
    );
    drop(held);
    assert!(!state.data_dir.join("convert").join(&id).exists());
    let (s, e) = get(&gw, &format!("/v1/documents/jobs/{id}/result")).await;
    assert_eq!(
        (s, e["error"]["code"].as_str()),
        (409, Some("job_cancelled"))
    );
}

#[tokio::test]
async fn a_job_in_flight_at_a_restart_is_interrupted_and_keeps_its_finished_files() {
    let (state, gw) = gateway().await;
    pool::configure(1);
    let held = pool::global().acquire().await;
    let id = start(
        &gw,
        json!([
            {"name": "a.txt", "data_base64": b64(b"one")},
            {"name": "b.txt", "data_base64": b64(b"two")},
        ]),
    )
    .await;
    // The job's task has read its first upload and waits for the slot: a
    // task that came later would find its upload gone with the stand-in
    // restart below and write that file's entry over this one.
    queued(&id).await;
    // The first file "finished" before the stop: its entry and row, as the
    // job's task writes them.
    let entry: Value = json!({
        "index": 0, "name": "a.txt", "status": "converted", "format": "text", "parts": 1,
        "content": "one", "warnings": [], "error": null,
        "stats": {"parts_by_source": {"text_layer": 0, "ocr": 0, "vision": 0, "native": 1},
                  "headings": [0,0,0,0,0,0], "tables": 0, "table_cells": 0, "pictures": 0,
                  "model_calls": 0, "time_ms": 1}
    });
    results::write(&state.data_dir, &id, serde_json::from_value(entry).unwrap())
        .await
        .unwrap();
    store::convert_jobs::file_done(&state.db, &id, 0, "converted", 1, None)
        .await
        .unwrap();
    // The restart: the next process finds the row still running.
    sweep::boot(&state.db, &state.data_dir).await;
    let (_, job) = get(&gw, &format!("/v1/documents/jobs/{id}")).await;
    assert_eq!(job["state"], "interrupted", "{job}");
    assert_eq!(job["documents"][0]["state"], "converted");
    assert_eq!(job["documents"][1]["state"], "interrupted");
    assert!(job["error"].as_str().unwrap().contains("stopped"));
    let (s, a) = get(&gw, &format!("/v1/documents/jobs/{id}/result")).await;
    assert_eq!(s, 200);
    assert_eq!(a["documents"][0]["content"], "one");
    assert_eq!(a["documents"][1]["error"]["code"], "interrupted");
    drop(held);
}

#[tokio::test]
async fn ended_jobs_go_after_the_retention_with_their_results() {
    let (state, gw) = gateway().await;
    let id = start(&gw, json!([{"name": "a.txt", "data_base64": b64(b"x")}])).await;
    let job = ended(&state, &gw, &id).await;
    assert_eq!(job["state"], "succeeded");
    assert_eq!(sweep::upkeep(&state).await, 0, "a fresh job stays");
    sqlx::query("UPDATE convert_jobs SET finished_at = '2000-01-01T00:00:00Z' WHERE id = ?1")
        .bind(&id)
        .execute(&state.db)
        .await
        .unwrap();
    assert_eq!(sweep::upkeep(&state).await, 1);
    assert!(!state.data_dir.join("convert").join(&id).exists());
    let (s, _) = get(&gw, &format!("/v1/documents/jobs/{id}")).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn the_usage_row_and_its_record_hold_no_content() {
    let (state, gw) = gateway().await;
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(&json!({
            "files": [{"name": "secret-name.txt", "data_base64": b64(b"secret words")}],
            "output": "text",
            "page_break": "secret marker",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let rows = store::convert_calls(&state.db, "/v1/documents/")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let all = format!("{:?}", rows[0]);
    assert!(
        all.contains(r#"\"name\":\"output\",\"value\":\"text\""#),
        "{all}"
    );
    assert!(
        all.contains(r#"\"name\":\"page_break\",\"value\":\"set\""#),
        "{all}"
    );
    for secret in ["secret-name", "secret words", "secret marker"] {
        assert!(!all.contains(secret), "{all}");
    }
}
