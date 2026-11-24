//! Security round A over HTTP (document-conversion design §14): a key's
//! jobs past "Jobs per key" are a 429, uploads past "Storage for conversion
//! jobs" a 507, a result past it a file not stored, each naming the
//! setting; docling's poll waits at most `POLL_WAIT_MAX_S` and says so;
//! the review round's cases: one key's jobs leave an answer share for
//! another key (B2), the storage count follows the disk (S2), the write
//! deadline in its header (S6), and the status WebSocket's message rate.

use std::time::Duration;

use base64::Engine as _;
use lmgw_core::config::hash_api_key;
use lmgw_core::extract::convert::budget::answers;
use lmgw_core::extract::convert::jobs::results;
use lmgw_core::extract::convert::pool;
use lmgw_core::state::{AppState, SharedState};
use serde_json::{json, Value};

use crate::chat_voice_speak::settings;
use crate::common::{serve, Gw};

async fn gateway() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn post_async(gw: &Gw, name: &str, bytes: &[u8]) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(&json!({"files": [{"name": name, "data_base64": b64(bytes)}], "async": true}))
        .send()
        .await
        .unwrap();
    let s = r.status().as_u16();
    (s, r.json().await.unwrap_or(Value::Null))
}

async fn ended(state: &SharedState, id: &str) {
    if let Some(mut rx) = state.convert_jobs.watch(id) {
        let _ = tokio::time::timeout(Duration::from_secs(30), async {
            while rx.changed().await.is_ok() {
                if state.convert_jobs.watch(id).is_none() {
                    break;
                }
            }
        })
        .await;
    }
}

#[tokio::test]
async fn a_key_past_its_jobs_is_a_429_that_names_the_setting() {
    let (state, gw) = gateway().await;
    settings(&state, |s| {
        s.documents.tool_slots = 1;
        s.documents.jobs_per_key = 1;
    })
    .await;
    // The pool's one slot is held here, so the first job stays queued.
    let held = pool::global().acquire().await;
    let (s, first) = post_async(&gw, "example-a.docx", &crate::chat_attach_kinds::docx("A")).await;
    assert_eq!(s, 202, "{first}");
    let (s, refused) = post_async(&gw, "example-b.txt", b"second").await;
    assert_eq!(s, 429, "{refused}");
    assert_eq!(refused["error"]["code"], "too_many_jobs", "{refused}");
    let m = refused["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("Jobs per key (1, documents_jobs_per_key)"),
        "{m}"
    );
    drop(held);
    ended(&state, first["job"].as_str().unwrap()).await;
    // Its place came back with its end.
    let (s, third) = post_async(&gw, "example-c.txt", b"third").await;
    assert_eq!(s, 202, "{third}");
    ended(&state, third["job"].as_str().unwrap()).await;
}

#[tokio::test]
async fn uploads_past_the_storage_bound_are_a_507_and_a_result_past_it_is_not_stored() {
    let (state, gw) = gateway().await;
    settings(&state, |s| s.documents.job_storage_mib = 1).await;
    let big = vec![b'x'; 2 << 20];
    let (s, refused) = post_async(&gw, "example-big.txt", &big).await;
    assert_eq!(s, 507, "{refused}");
    assert_eq!(refused["error"]["code"], "job_storage_full");
    let m = refused["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("Storage for conversion jobs (1.0 MiB, documents_job_storage_mib)"),
        "{m}"
    );
    // 768 KiB of quoted text fits as an upload, but its result does not:
    // every quote is escaped in the stored entry (the upload has left the
    // count when its file is done, review S2).
    let text = "\"\" ".repeat(256 << 10);
    let (s, accepted) = post_async(&gw, "example-text.txt", text.as_bytes()).await;
    assert_eq!(s, 202, "{accepted}");
    let id = accepted["job"].as_str().unwrap().to_string();
    ended(&state, &id).await;
    let r = gw
        .client()
        .get(format!("{gw}/v1/documents/jobs/{id}/result"))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let e = &body["documents"][0]["error"];
    let m = e["message"].as_str().unwrap_or_default();
    assert!(
        m.contains("Storage for conversion jobs") && m.contains("job_storage_full"),
        "{body}"
    );
    // A small one still fits once the bound is lifted.
    settings(&state, |s| s.documents.job_storage_mib = 0).await;
    let (s, _) = post_async(&gw, "example-small.txt", b"small").await;
    assert_eq!(s, 202);
}

#[tokio::test]
async fn a_poll_waits_at_most_its_bound_and_says_so() {
    let (state, gw) = gateway().await;
    let form = reqwest::multipart::Form::new().part(
        "files",
        reqwest::multipart::Part::bytes(b"# Poll\n\nbody".to_vec()).file_name("example.md"),
    );
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/file/async"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let task: Value = r.json().await.unwrap();
    let id = task["task_id"].as_str().unwrap().to_string();
    ended(&state, &id).await;
    let r = gw
        .client()
        .get(format!("{gw}/v1/status/poll/{id}?wait=100000"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        r.headers()["x-lmgw-poll-wait"].to_str().unwrap(),
        "120 (POLL_WAIT_MAX_S)"
    );
    let r = gw
        .client()
        .get(format!("{gw}/v1/status/poll/{id}?wait=1"))
        .send()
        .await
        .unwrap();
    assert!(r.headers().get("x-lmgw-poll-wait").is_none());
}

#[tokio::test]
async fn settings_documents_shows_the_bounds() {
    let (_state, gw) = gateway().await;
    let r = gw
        .client()
        .post(format!("{gw}/api/op/doc_tools"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let caps: Value = r.json().await.unwrap();
    let a = &caps["answers"];
    assert_eq!(a["output_bytes"], 1u64 << 30, "{a}");
    assert_eq!(a["write_grace_s"], 60);
    assert_eq!(a["read_grace_s"], 60);
    // Auth off: one caller, the shares off.
    assert_eq!(a["callers"]["shared"], false, "{a}");
    let b = &caps["job_bounds"];
    assert!(b["per_key"].as_u64().unwrap() >= 1, "{b}");
    assert_eq!(b["per_key"], b["pool_slots"]);
    assert_eq!(b["storage_by_hand"], false);
    assert_eq!(caps["pool"]["per_caller_slots"], caps["pool"]["slots"]);
}

/// Two client keys beside the owner, auth on: the shares apply.
async fn two_keys(state: &SharedState) {
    settings(state, |s| s.auth_enabled = true).await;
    for (name, key) in [("example-a", KEY_A), ("example-b", KEY_B)] {
        sqlx::query(
            "INSERT INTO api_keys (name, key_hash, enabled, scope_mode, scope_patterns,
                 budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit)
             VALUES (?1, ?2, 1, 'all', '', 0, 'month', 0, 0, 0)",
        )
        .bind(name)
        .bind(hash_api_key(key))
        .execute(&state.db)
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
}

const KEY_A: &str = "lmgw-example-key-a";
const KEY_B: &str = "lmgw-example-key-b";

async fn post_as(gw: &Gw, key: &str, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("{gw}/v1/documents/convert"))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .unwrap();
    let s = r.status().as_u16();
    (s, r.json().await.unwrap_or(Value::Null))
}

/// Security round A review B2: four async jobs of one
/// key at a bound of four answer shares hold three, and another key's
/// 12-byte text gets the fourth at once.
#[tokio::test]
async fn one_keys_jobs_leave_an_answer_share_for_another_key() {
    let (state, gw) = gateway().await;
    two_keys(&state).await;
    let caps: Value = gw
        .client()
        .post(format!("{gw}/api/op/doc_tools"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        caps["answers"]["callers"]["shared"], true,
        "{}",
        caps["answers"]
    );
    let answers = answers::global();
    answers.set_limit(Some((4u64 << 30) + (64 << 20)));
    settings(&state, |s| {
        s.documents.tool_slots = 1;
        s.documents.jobs_per_key = 8;
    })
    .await;
    // The pool's one slot is held here: the jobs stay queued with their
    // shares.
    let held = pool::global().acquire().await;
    let job = |name: &str| json!({"files": [{"name": name, "data_base64": b64(b"# Example\n\ntext")}], "async": true});
    let mut ids = Vec::new();
    for i in 0..3 {
        let (s, v) = post_as(&gw, KEY_A, job(&format!("example-{i}.md"))).await;
        assert_eq!(s, 202, "{v}");
        ids.push(v["job"].as_str().unwrap().to_string());
    }
    // The fourth waits for a share of its own key's.
    let fourth = {
        let gw = Gw {
            base: gw.base.clone(),
            key: gw.key.clone(),
        };
        tokio::spawn(async move { post_as(&gw, KEY_A, job("example-3.md")).await })
    };
    let victim = {
        let gw = Gw {
            base: gw.base.clone(),
            key: gw.key.clone(),
        };
        tokio::spawn(async move {
            let body =
                json!({"files": [{"name": "small.txt", "data_base64": b64(b"twelve bytes")}]});
            post_as(&gw, KEY_B, body).await
        })
    };
    let mut seen = None;
    for _ in 0..200 {
        let f = answers.facts();
        if victim.is_finished() || (f.holding == 4 && f.waiting == 1) {
            seen = Some(f);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let f = seen.expect("the other key got the fourth share");
    assert!(
        victim.is_finished() || (f.holding, f.waiting) == (4, 1),
        "{f:?}"
    );
    if !victim.is_finished() {
        // Who holds the fourth share (verification SF3): first come, first
        // served would have handed it to key A's fourth request, which
        // answers its 202 at once. Under the rule key B holds it and A's
        // fourth still waits, here and a moment later.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            victim.is_finished() || !fourth.is_finished(),
            "key A's fourth request took the share key B was owed: {:?}",
            answers.facts()
        );
    }
    drop(held);
    let (s, v) = tokio::time::timeout(Duration::from_secs(60), victim)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s, 200, "{v}");
    let (s, v) = fourth.await.unwrap();
    assert_eq!(s, 202, "{v}");
    ids.push(v["job"].as_str().unwrap().to_string());
    for id in &ids {
        ended(&state, id).await;
    }
    answers.set_limit(None);
}

/// Review S2: the jobs' storage count equals what is on disk after their
/// files are done and the jobs ended.
#[tokio::test]
async fn the_storage_count_equals_the_disk_after_jobs() {
    let (state, gw) = gateway().await;
    let mut ids = Vec::new();
    for i in 0..4 {
        let text = format!("# Example {i}\n\n{}", "word ".repeat(20_000));
        let (s, v) = post_async(&gw, &format!("example-{i}.md"), text.as_bytes()).await;
        assert_eq!(s, 202, "{v}");
        ids.push(v["job"].as_str().unwrap().to_string());
    }
    for id in &ids {
        ended(&state, id).await;
    }
    let (_, on_disk) = results::usage(&state.data_dir).await;
    assert!(on_disk > 0);
    assert_eq!(state.convert_jobs.ledger.stored(), on_disk);
}

/// Review S6: a sync answer names its write deadline.
#[tokio::test]
async fn an_answer_names_its_write_deadline() {
    let (_state, gw) = gateway().await;
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(&json!({"files": [{"name": "example.md", "data_base64": b64(b"# Title")}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        r.headers()["x-lmgw-write-deadline"].to_str().unwrap(),
        "60 (ANSWER_WRITE_GRACE_S + 1 s per ANSWER_MIN_RATE_BYTES_PER_S)"
    );
}

/// Security round A review, an open point: a status WebSocket whose client sends
/// more than `WS_CLIENT_MESSAGES_PER_S` messages a second is closed with
/// 1008, the reason naming the bound.
#[tokio::test]
async fn a_websocket_past_its_message_rate_is_closed_with_1008() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let (state, gw) = gateway().await;
    settings(&state, |s| s.documents.tool_slots = 1).await;
    let held = pool::global().acquire().await;
    let r = gw
        .client()
        .post(format!("{gw}/v1/convert/source/async"))
        .json(&json!({"sources": [{"kind": "file", "base64_string": b64(b"# Title"), "filename": "example.md"}]}))
        .send()
        .await
        .unwrap();
    let task: Value = r.json().await.unwrap();
    let id = task["task_id"].as_str().unwrap().to_string();
    let url = format!(
        "{}/v1/status/ws/{id}?api_key={}",
        gw.base.replacen("http", "ws", 1),
        gw.key
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    for _ in 0..20 {
        if ws.send(Message::Text("status?".into())).await.is_err() {
            break;
        }
    }
    let mut closed = None;
    while let Ok(Some(m)) = tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
        match m {
            Ok(Message::Close(f)) => {
                closed = f;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let f = closed.expect("a close frame");
    assert_eq!(u16::from(f.code), 1008);
    assert!(
        f.reason.contains("WS_CLIENT_MESSAGES_PER_S"),
        "{}",
        f.reason
    );
    drop(held);
    ended(&state, &id).await;
}

/// Verification N-b: a client that reads part of a large answer and goes
/// away leaves a 499 `client_disconnected` row that says how much was
/// handed over, not a plain 200.
#[tokio::test]
async fn an_answer_whose_client_left_midway_is_a_499_row() {
    let (state, gw) = gateway().await;
    // 48 MiB of text: more than the connection's buffers hold.
    let text = "example words ".repeat(48 << 20 >> 4);
    let ct = "multipart/form-data; boundary=example-b";
    let mut body = b"--example-b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"example.txt\"\r\nContent-Type: text/plain\r\n\r\n".to_vec();
    body.extend_from_slice(text.as_bytes());
    body.extend_from_slice(b"\r\n--example-b--\r\n");
    settings(&state, |s| s.max_body_mb = 128).await;
    let mut r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .header("content-type", ct)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let first = r.chunk().await.unwrap().unwrap();
    assert!(!first.is_empty());
    drop(r);
    let rows = crate::documents_convert::doc_rows(&state, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].status, 499, "{:?}", rows[0]);
    assert_eq!(rows[0].error_kind.as_deref(), Some("client_disconnected"));
    let m = rows[0].error_msg.as_deref().unwrap_or("");
    assert!(m.contains("handed over (the answer was 200)"), "{m}");
}
