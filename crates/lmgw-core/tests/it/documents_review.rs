//! WP5's review round (document-conversion design §14, WP5): each test
//! fails on the code the review found wrong. Key isolation on every job
//! route, the query-string key on the WebSocket route only, floats no
//! duration holds, the stray sweep beside a job that starts, the key's
//! concurrency limit and request budget against async jobs, uploads kept on
//! disk (private) while a job is queued, a failed write, a start that
//! fails, `abort_on_error` in the counts, the zip's names, the option
//! header's size, and the task result of a failed job.

use futures::StreamExt;
use lmgw_core::extract::convert::jobs::{self, results, sweep};
use lmgw_core::extract::convert::pool;
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::common::Gw;
use crate::docling_serve::{b64, gateway};

/// A gateway with auth on and keys `example-a` and `example-b`.
async fn keyed() -> (SharedState, Gw) {
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
    (state, gw)
}

fn with(key: &str) -> reqwest::Client {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert("x-api-key", key.parse().unwrap());
    reqwest::Client::builder()
        .default_headers(h)
        .build()
        .unwrap()
}

fn native_body() -> Value {
    json!({"files": [{"name": "a.txt", "data_base64": b64(b"one")}], "async": true})
}

fn docling_body() -> Value {
    json!({"sources": [{"kind": "file", "base64_string": b64(b"# T\n\nB"), "filename": "a.md"}]})
}

async fn native_job(c: &reqwest::Client, gw: &Gw) -> String {
    let r = c
        .post(format!("{gw}/v1/documents/convert"))
        .json(&native_body())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 202);
    r.json::<Value>().await.unwrap()["job"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn docling_task(c: &reqwest::Client, gw: &Gw) -> String {
    let r = c
        .post(format!("{gw}/v1/convert/source/async"))
        .json(&docling_body())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    r.json::<Value>().await.unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Wait for job `id` to end (it converts text here).
async fn ended(state: &SharedState, id: &str) {
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
}

async fn status(c: &reqwest::Client, url: String) -> u16 {
    c.get(url).send().await.unwrap().status().as_u16()
}

async fn fetched_at(state: &SharedState, id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT fetched_at FROM convert_jobs WHERE id = ?1")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn another_key_reaches_no_job_route_and_marks_nothing() {
    let (state, gw) = keyed().await;
    let (a, b) = (with("lmgw-example-a"), with("lmgw-example-b"));
    let id = native_job(&a, &gw).await;
    ended(&state, &id).await;
    let job = format!("{gw}/v1/documents/jobs/{id}");
    assert_eq!(status(&b, format!("{job}/result")).await, 404);
    assert_eq!(status(&b, format!("{job}/result?index=0")).await, 404);
    assert_eq!(status(&b, job.clone()).await, 404);
    let r = b.delete(&job).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 404);
    assert_eq!(
        status(&a, format!("{job}/result?index=0")).await,
        200,
        "still there"
    );

    let task = docling_task(&a, &gw).await;
    ended(&state, &task).await;
    assert_eq!(
        status(&b, format!("{gw}/v1/status/poll/{task}?wait=0")).await,
        404
    );
    assert_eq!(status(&b, format!("{gw}/v1/result/{task}")).await, 404);
    assert_eq!(
        fetched_at(&state, &task).await,
        None,
        "a foreign fetch marks nothing"
    );
    // The WebSocket, with the other key as docling's query parameter.
    let url = format!(
        "{}/v1/status/ws/{task}?api_key=lmgw-example-b",
        gw.base.replacen("http", "ws", 1)
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let first = loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Text(t) => break serde_json::from_str::<Value>(&t).unwrap(),
            _ => continue,
        }
    };
    assert_eq!(first["message"], "error");
    assert_eq!(first["error"], "Task not found.");
    // The owner of the task fetches, and the clock starts once.
    assert_eq!(status(&a, format!("{gw}/v1/result/{task}")).await, 200);
    let first = fetched_at(&state, &task).await;
    assert!(first.is_some());
    assert_eq!(status(&a, format!("{gw}/v1/result/{task}")).await, 200);
    assert_eq!(fetched_at(&state, &task).await, first);
}

#[tokio::test]
async fn the_second_fetch_does_not_start_the_removal_clock_again() {
    let (state, gw) = gateway().await;
    let t = docling_task(&gw.client(), &gw).await;
    ended(&state, &t).await;
    let (_, first) = store::convert_jobs::mark_fetched(&state.db, &t)
        .await
        .unwrap();
    let (at, again) = store::convert_jobs::mark_fetched(&state.db, &t)
        .await
        .unwrap();
    assert!(first && !again && at.is_some());
}

#[tokio::test]
async fn a_key_in_the_query_opens_nothing_but_the_websocket() {
    let (state, gw) = keyed().await;
    let a = with("lmgw-example-a");
    let id = native_job(&a, &gw).await;
    ended(&state, &id).await;
    let bare = reqwest::Client::new();
    for path in [
        format!("/v1/documents/jobs/{id}"),
        format!("/v1/status/poll/{id}"),
        format!("/v1/result/{id}"),
        "/v1/models".to_string(),
    ] {
        let url = format!("{gw}{path}?api_key=lmgw-example-a");
        assert_eq!(status(&bare, url).await, 401, "{path}");
    }
}

#[tokio::test]
async fn numbers_no_duration_holds_are_answered_not_panicked_on() {
    let (state, gw) = gateway().await;
    let t = docling_task(&gw.client(), &gw).await;
    ended(&state, &t).await;
    // The longest wait the route allows: the task has ended, so it answers.
    let r = gw
        .client()
        .get(format!("{gw}/v1/status/poll/{t}?wait=1e300"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["task_status"], "success");
    // A timeout none can hold is docling's 422, JSON and form.
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/source"))
        .json(
            &json!({"sources": docling_body()["sources"], "options": {"document_timeout": 1e300}}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        v["detail"][0]["loc"],
        json!(["body", "options", "document_timeout"])
    );
    let form = reqwest::multipart::Form::new()
        .part(
            "files",
            reqwest::multipart::Part::bytes(b"x".to_vec()).file_name("a.txt"),
        )
        .text("document_timeout", "1e300");
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/file"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 422);
}

fn job_dir(state: &SharedState, id: &str) -> std::path::PathBuf {
    state.data_dir.join("convert").join(id)
}

#[tokio::test]
async fn the_sweep_keeps_a_live_job_whose_row_it_has_not_read_and_removes_a_stray() {
    let (state, gw) = gateway().await;
    pool::configure(1);
    let held = pool::global().acquire().await;
    let id = native_job(&gw.client(), &gw).await;
    // The window: the directory exists, the row is not in what the sweep
    // read.
    sqlx::query("DELETE FROM convert_jobs WHERE id = ?1")
        .bind(&id)
        .execute(&state.db)
        .await
        .unwrap();
    let stray = "cj_00112233445566778899aabbccddeeff";
    results::write_input(&state.data_dir, stray, 0, b"x")
        .await
        .unwrap();
    sweep::upkeep(&state).await;
    assert!(
        job_dir(&state, &id).exists(),
        "a live job's directory stays"
    );
    assert!(
        !job_dir(&state, stray).exists(),
        "a directory nothing owns goes"
    );
    drop(held);
    ended(&state, &id).await;
}

async fn set_limits(state: &SharedState, name: &str, rpm: i64, concurrency: i64) {
    sqlx::query("UPDATE api_keys SET rpm_limit = ?2, concurrency_limit = ?3 WHERE name = ?1")
        .bind(name)
        .bind(rpm)
        .bind(concurrency)
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
}

#[tokio::test]
async fn a_key_at_its_concurrency_limit_runs_one_job_and_reads_status_freely() {
    let (state, gw) = keyed().await;
    set_limits(&state, "example-a", 0, 1).await;
    set_limits(&state, "example-b", 3, 0).await;
    let (a, b) = (with("lmgw-example-a"), with("lmgw-example-b"));
    pool::configure(1);
    let held = pool::global().acquire().await;
    let id = native_job(&a, &gw).await;
    // The job holds the key's slot: a second request is refused while it runs.
    let r = a
        .post(format!("{gw}/v1/documents/convert"))
        .json(&native_body())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 429);
    // Looking at the job costs no slot and no request.
    for _ in 0..5 {
        assert_eq!(
            status(&a, format!("{gw}/v1/documents/jobs/{id}")).await,
            200
        );
        assert_eq!(
            status(&a, format!("{gw}/v1/status/poll/{id}?wait=0")).await,
            200
        );
    }
    // A key with 3 requests a minute: one job, then many polls.
    let other = native_job(&b, &gw).await;
    for _ in 0..8 {
        assert_eq!(
            status(&b, format!("{gw}/v1/status/poll/{other}?wait=0")).await,
            200
        );
    }
    drop(held);
    ended(&state, &id).await;
    // The slot is given back when the job ends.
    let mut accepted = false;
    for _ in 0..50 {
        let r = a
            .post(format!("{gw}/v1/documents/convert"))
            .json(&native_body())
            .send()
            .await
            .unwrap();
        if r.status().as_u16() == 202 {
            accepted = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(accepted, "the slot was not given back");
}

#[tokio::test]
async fn a_queued_job_keeps_its_upload_on_disk_privately_and_the_queue_shows() {
    let (state, gw) = gateway().await;
    pool::configure(1);
    let held = pool::global().acquire().await;
    let id = native_job(&gw.client(), &gw).await;
    let input = job_dir(&state, &id).join("input").join("0");
    assert_eq!(std::fs::read(&input).unwrap(), b"one");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&input), 0o600);
        assert_eq!(mode(&job_dir(&state, &id)), 0o700);
    }
    let v: Value = gw
        .client()
        .post(format!("{gw}/api/op/doc_jobs_stored"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (v["jobs"].as_u64(), v["running"].as_u64()),
        (Some(1), Some(1)),
        "{v}"
    );
    assert_eq!(v["queued_files"], 1);
    assert!(v["bytes"].as_u64().unwrap() >= 3);
    drop(held);
    ended(&state, &id).await;
    assert!(!input.exists(), "a finished file's upload goes");
    assert!(job_dir(&state, &id).join("0").join("entry.json").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::metadata(job_dir(&state, &id).join("0").join("entry.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o600);
    }
    // Cancelling a job deletes its uploads with it.
    let held = pool::global().acquire().await;
    let id = native_job(&gw.client(), &gw).await;
    let r = gw
        .client()
        .delete(format!("{gw}/v1/documents/jobs/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(!job_dir(&state, &id).exists());
    drop(held);
}

#[tokio::test]
async fn a_result_that_cannot_be_written_is_that_files_failure_not_a_restart() {
    let (state, gw) = gateway().await;
    pool::configure(1);
    let held = pool::global().acquire().await;
    let id = native_job(&gw.client(), &gw).await;
    // `<job>/0` is a file, so the entry's directory cannot be made.
    std::fs::write(job_dir(&state, &id).join("0"), b"in the way").unwrap();
    drop(held);
    ended(&state, &id).await;
    let (_, job) = {
        let r = gw
            .client()
            .get(format!("{gw}/v1/documents/jobs/{id}"))
            .send()
            .await
            .unwrap();
        (r.status(), r.json::<Value>().await.unwrap())
    };
    assert_eq!(job["documents"][0]["state"], "failed", "{job}");
    assert!(job["documents"][0]["error"]
        .as_str()
        .unwrap()
        .starts_with("result_not_stored"));
    let e: Value = gw
        .client()
        .get(format!("{gw}/v1/documents/jobs/{id}/result?index=0"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let said = e["error"]["message"].as_str().unwrap();
    assert!(
        said.contains("could not be stored") && !said.contains("gateway stopped"),
        "{e}"
    );
}

#[tokio::test]
async fn a_job_that_cannot_start_leaves_no_running_row() {
    let (state, gw) = gateway().await;
    // `convert` is a file, so no job directory can be made.
    std::fs::create_dir_all(&state.data_dir).unwrap();
    std::fs::write(state.data_dir.join("convert"), b"in the way").unwrap();
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(&native_body())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 500);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM convert_jobs")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn abort_on_error_fails_a_partial_file_in_the_job_counts_too() {
    let (state, _gw) = gateway().await;
    let partial: lmgw_api_types::documents::convert::ConvertedDocument =
        serde_json::from_value(json!({
            "index": 0, "name": "a.pdf", "status": "partial", "format": "pdf", "parts": 2,
            "content": "page one", "warnings": [], "error": null,
            "stats": {"parts_by_source": {"text_layer": 1, "ocr": 0, "vision": 0, "native": 0},
                      "headings": [0,0,0,0,0,0], "tables": 0, "table_cells": 0, "pictures": 0,
                      "model_calls": 0, "time_ms": 1}
        }))
        .unwrap();
    for (abort, want) in [(true, "failed"), (false, "partial")] {
        let id = format!("cj_{}", hex::encode(rand::random::<[u8; 16]>()));
        store::convert_jobs::insert_job(
            &state.db,
            &store::convert_jobs::NewJob {
                id: &id,
                key_id: None,
                origin: "docling",
                created_at: chrono::Utc::now(),
                input_bytes: 0,
                warnings: "[]",
                adapter: None,
            },
            &[store::convert_jobs::NewJobFile {
                idx: 0,
                name: "a.pdf".into(),
                bytes: 1,
                state: "pending",
            }],
        )
        .await
        .unwrap();
        jobs::record_file(&state, &id, partial.clone(), abort).await;
        let files = store::convert_jobs::job_files(&state.db, &id)
            .await
            .unwrap();
        assert_eq!(files[0].state, want);
        let stored = results::read(&state.data_dir, &id, 0).await.unwrap();
        assert_eq!(serde_json::to_value(stored.status).unwrap(), want);
    }
}

#[tokio::test]
async fn the_zip_keeps_every_file_under_names_that_stay_unique_and_says_so() {
    let (_state, gw) = gateway().await;
    let mut form = reqwest::multipart::Form::new();
    for name in ["report.txt", "report_2.txt", "report.txt", "REPORT.txt"] {
        form = form.part(
            "files",
            reqwest::multipart::Part::bytes(b"text".to_vec()).file_name(name),
        );
    }
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/file"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        r.headers()["x-lmgw-documents"],
        "converted=4, partial=0, failed=0, skipped=0, entry_names_renamed=2"
    );
    let bytes = r.bytes().await.unwrap();
    let z = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
    let mut names: Vec<&str> = z.file_names().collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["REPORT_3.md", "report.md", "report_2.md", "report_2_2.md"]
    );
}

#[tokio::test]
async fn the_option_notes_header_stays_small_whatever_names_are_sent() {
    let (_state, gw) = gateway().await;
    let mut options = serde_json::Map::new();
    for i in 0..2_000 {
        options.insert(format!("example_{i}_{}", "x".repeat(300)), json!(1));
    }
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/source"))
        .json(&json!({"sources": docling_body()["sources"], "options": options}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let h = r.headers()["x-lmgw-option-notes"].to_str().unwrap();
    assert!(
        h.len() <= lmgw_core::extract::convert::calls::NOTES_HEADER_MAX_BYTES,
        "{}",
        h.len()
    );
    assert!(h.contains("unknown_options=not_yet(1968 more"), "{h}");
}

#[tokio::test]
async fn a_task_that_failed_as_a_whole_is_a_200_task_failure_result() {
    let (state, gw) = gateway().await;
    let png = |name: &str| json!({"kind": "file", "base64_string": b64(b"\x89PNG\r\n\x1a\nrest"), "filename": name});
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/source/batch"))
        .json(&json!({"sources": [png("a.png"), png("b.png")], "target": {"kind": "zip"}}))
        .send()
        .await
        .unwrap();
    let t = r.json::<Value>().await.unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    ended(&state, &t).await;
    let poll: Value = gw
        .client()
        .get(format!("{gw}/v1/status/poll/{t}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(poll["task_status"], "failure");
    let r = gw
        .client()
        .get(format!("{gw}/v1/result/{t}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["kind"], "TaskFailureResult");
    assert!(v["failure"]["message"]
        .as_str()
        .unwrap()
        .contains("No documents were exported"));
}

#[tokio::test]
async fn a_file_waiting_for_a_slot_is_not_late() {
    let (_state, gw) = gateway().await;
    pool::configure(1);
    let held = pool::global().acquire().await;
    let c = gw.client();
    let gw2 = gw.base.clone();
    let req = tokio::spawn(async move {
        c.post(format!("{gw2}/v1/convert/source"))
            .json(&json!({"sources": docling_body()["sources"], "options": {"document_timeout": 0.2}}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    });
    // Longer than the timeout, with no slot to be had.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    drop(held);
    let v = req.await.unwrap();
    assert_eq!(v["status"], "success", "{v}");
}
