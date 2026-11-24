//! docling-serve's task routes over lmgw's jobs (document-conversion design
//! §7.4.1, D24, WP5): an async request polled to completion, its result in
//! the requested shape, docling's single-use results, and the status
//! WebSocket from the same job state.

use futures::{SinkExt, StreamExt};
use lmgw_core::extract::convert::jobs::sweep;
use lmgw_core::extract::convert::pool;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::common::Gw;
use crate::docling_serve::{b64, gateway};

async fn start(gw: &Gw, route: &str, body: Value) -> Value {
    let r = gw
        .client()
        .post(format!("{gw}{route}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    r.json().await.unwrap()
}

async fn poll(gw: &Gw, id: &str, wait: u32) -> Value {
    gw.client()
        .get(format!("{gw}/v1/status/poll/{id}?wait={wait}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn one_file() -> Value {
    json!({"sources": [{"kind": "file", "base64_string": b64(b"# Title\n\nBody."), "filename": "example.md"}]})
}

#[tokio::test]
async fn an_async_task_is_polled_to_success_and_its_result_is_single_use() {
    let (state, gw) = gateway().await;
    let t = start(&gw, "/v1/convert/source/async", one_file()).await;
    let keys: Vec<&str> = t.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "error_message",
            "failure",
            "task_id",
            "task_meta",
            "task_position",
            "task_status",
            "task_type"
        ]
    );
    assert_eq!(t["task_type"], "convert");
    let id = t["task_id"].as_str().unwrap().to_string();
    let done = poll(&gw, &id, 30).await;
    assert_eq!(done["task_status"], "success", "{done}");
    assert_eq!(done["task_meta"]["num_docs"], 1);
    assert_eq!(done["task_meta"]["num_succeeded"], 1);
    let result = || async {
        gw.client()
            .get(format!("{gw}/v1/result/{id}"))
            .send()
            .await
            .unwrap()
    };
    let r = result().await;
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["document"]["md_content"], "# Title\n\nBody.");
    assert_eq!(v["status"], "success");
    // docling's single-use results: a second fetch within the removal delay
    // still answers; after it the task is gone.
    assert_eq!(result().await.status().as_u16(), 200);
    sweep::upkeep(&state).await;
    assert_eq!(result().await.status().as_u16(), 200, "not yet 300 s");
    sqlx::query("UPDATE convert_jobs SET fetched_at = '2000-01-01T00:00:00Z' WHERE id = ?1")
        .bind(&id)
        .execute(&state.db)
        .await
        .unwrap();
    sweep::upkeep(&state).await;
    let r = result().await;
    assert_eq!(r.status().as_u16(), 404);
    assert_eq!(
        r.json::<Value>().await.unwrap()["detail"],
        "Task not found."
    );
}

#[tokio::test]
async fn a_running_task_has_no_result_yet_and_the_batch_route_zips() {
    let (_state, gw) = gateway().await;
    pool::configure(1);
    let held = pool::global().acquire().await;
    let body = json!({
        "sources": [
            {"kind": "file", "base64_string": b64(b"one"), "filename": "a.txt"},
            {"kind": "file", "base64_string": b64(b"two"), "filename": "b.txt"},
        ],
        "target": {"kind": "zip"},
    });
    let t = start(&gw, "/v1/convert/source/batch", body).await;
    assert_eq!(t["task_status"], "pending");
    let id = t["task_id"].as_str().unwrap().to_string();
    let r = gw
        .client()
        .get(format!("{gw}/v1/result/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
    assert_eq!(
        r.json::<Value>().await.unwrap()["detail"],
        "Task result not found. Please wait for a completion status."
    );
    drop(held);
    assert_eq!(poll(&gw, &id, 30).await["task_status"], "success");
    let r = gw
        .client()
        .get(format!("{gw}/v1/result/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["content-type"], "application/zip");
    assert_eq!(
        r.headers()["x-lmgw-documents"],
        "converted=2, partial=0, failed=0, skipped=0"
    );
}

#[tokio::test]
async fn an_unknown_task_is_not_found_on_every_task_route() {
    let (_state, gw) = gateway().await;
    for path in [
        "/v1/status/poll/cj_00",
        "/v1/result/cj_00",
        "/v1/status/poll/../x",
    ] {
        let r = gw.client().get(format!("{gw}{path}")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 404, "{path}");
    }
}

fn ws_url(gw: &Gw, id: &str) -> String {
    format!(
        "{}/v1/status/ws/{id}?api_key={}",
        gw.base.replacen("http", "ws", 1),
        gw.key
    )
}

async fn next(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Option<Value> {
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(30), ws.next())
            .await
            .ok()??
        {
            Ok(Message::Text(t)) => return serde_json::from_str(&t).ok(),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

#[tokio::test]
async fn the_websocket_pushes_the_state_until_the_task_completes() {
    let (_state, gw) = gateway().await;
    pool::configure(1);
    let held = pool::global().acquire().await;
    let t = start(&gw, "/v1/convert/source/async", one_file()).await;
    let id = t["task_id"].as_str().unwrap().to_string();
    // The key comes as docling's query parameter.
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(&gw, &id))
        .await
        .unwrap();
    let first = next(&mut ws).await.unwrap();
    assert_eq!(first["message"], "connection");
    assert_eq!(first["task"]["task_status"], "pending");
    let second = next(&mut ws).await.unwrap();
    assert_eq!(
        (
            second["message"].as_str(),
            second["task"]["task_status"].as_str()
        ),
        (Some("update"), Some("pending"))
    );
    // A client message asks for an update, as in docling.
    ws.send(Message::Text("status?".into())).await.unwrap();
    assert_eq!(
        next(&mut ws).await.unwrap()["task"]["task_status"],
        "pending"
    );
    drop(held);
    let mut last = Value::Null;
    while let Some(v) = next(&mut ws).await {
        last = v;
    }
    assert_eq!(last["message"], "update");
    assert_eq!(last["task"]["task_status"], "success", "{last}");

    // An unknown task: one error frame, then the close.
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(&gw, "cj_00"))
        .await
        .unwrap();
    let e = next(&mut ws).await.unwrap();
    assert_eq!(
        e,
        json!({"message": "error", "task": null, "error": "Task not found."})
    );
    assert!(next(&mut ws).await.is_none());
}
