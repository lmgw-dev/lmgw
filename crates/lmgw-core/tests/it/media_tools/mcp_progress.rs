//! `/mcp` while a picture is drawn (design §7.2, §7.3): a `tools/call` with
//! a `progressToken` from a client that takes SSE is answered as a stream
//! of rising `notifications/progress` and then the response; without
//! either it stays one JSON body; `notifications/cancelled` for the request
//! cancels the job.

use serde_json::{json, Value};
use wiremock::MockServer;

use super::chat_turn::until;
use super::{
    gateway, gateway_recording, generating, image_row, job, mount_sd, submitted, Gw, Z_IMAGE,
};

/// An `/mcp` session's id.
async fn session(gw: &Gw) -> String {
    let resp = gw
        .client()
        .post(format!("{gw}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": { "name": "test", "version": "0" } }
        }))
        .send()
        .await
        .unwrap();
    let sid = resp.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    let _ = resp.text().await;
    sid
}

/// `tools/call` of `image__generate` as request 2 with `meta`, `accept`:
/// the answer's content type and body.
async fn draw(gw: &Gw, sid: &str, accept: &str, meta: Option<Value>) -> (String, String) {
    let mut params = json!({"name": "image__generate",
                            "arguments": {"model": "image/z", "prompt": "a red square"}});
    if let Some(m) = meta {
        params["_meta"] = m;
    }
    let resp = gw
        .client()
        .post(format!("{gw}/mcp"))
        .header("accept", accept)
        .header("mcp-session-id", sid)
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": params}))
        .send()
        .await
        .unwrap();
    let ct = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    (ct, resp.text().await.unwrap())
}

/// The JSON-RPC messages of an SSE body, in order.
fn messages(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| serde_json::from_str(d.trim()).unwrap())
        .collect()
}

const BOTH: &str = "application/json, text/event-stream";

#[tokio::test]
async fn a_call_with_a_progress_token_streams_rising_progress_then_its_answer() {
    let sd = MockServer::start().await;
    mount_sd(
        &sd,
        &[generating(1, 3, ""), generating(1, 5, ""), job("completed")],
    )
    .await;
    let (_state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let sid = session(&gw).await;
    let (ct, body) = draw(&gw, &sid, BOTH, Some(json!({"progressToken": "t-1"}))).await;
    assert!(ct.starts_with("text/event-stream"), "{ct}");
    let msgs = messages(&body);
    let (last, notes) = msgs.split_last().expect("messages");
    assert_eq!(last["id"], json!(2), "the answer comes last: {body}");
    assert_eq!(last["result"]["isError"], json!(false), "{body}");
    assert!(notes.len() >= 3, "{body}");
    let mut before = -1.0;
    for n in notes {
        assert_eq!(n["method"], json!("notifications/progress"), "{n}");
        assert_eq!(n["params"]["progressToken"], json!("t-1"));
        let p = n["params"]["progress"].as_f64().unwrap();
        assert!(p > before, "progress only rises: {body}");
        before = p;
    }
    assert_eq!(notes[0]["params"]["message"], json!("loading image/z"));
    let steps: Vec<&Value> = notes
        .iter()
        .filter(|n| n["params"]["total"] == json!(8))
        .collect();
    assert_eq!(steps.len(), 2, "{body}");
    assert_eq!(steps[1]["params"]["progress"], json!(5.0));
    assert_eq!(steps[1]["params"]["message"], json!("image/z: step 5 / 8"));
    // sd-server reports steps only beside a preview: the cheapest is asked
    // for, though MCP progress carries no image.
    assert_eq!(submitted(&sd).await[0]["preview"], json!("proj"));
}

#[tokio::test]
async fn without_a_token_or_a_stream_the_answer_stays_one_json_body() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[generating(1, 3, ""), job("completed")]).await;
    let (_state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let sid = session(&gw).await;
    for (accept, meta) in [
        (BOTH, None),
        ("application/json", Some(json!({"progressToken": 7}))),
    ] {
        let (ct, body) = draw(&gw, &sid, accept, meta).await;
        assert!(ct.starts_with("application/json"), "{ct}: {body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["result"]["isError"], json!(false), "{body}");
    }
    // Nobody follows their steps, but the job's pace is read from them:
    // the quick sketch is asked for all the same (§5.2).
    assert!(submitted(&sd)
        .await
        .iter()
        .all(|b| b["preview"] == json!("proj")));
}

#[tokio::test]
async fn a_cancelled_notification_cancels_the_running_job() {
    let sd = MockServer::start().await;
    // Generating for as long as it is asked.
    mount_sd(&sd, &[generating(1, 3, "")]).await;
    let (_state, gw, podman) = gateway_recording(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let sid = session(&gw).await;
    let call = tokio::spawn({
        let (gw, sid) = (gw.clone(), sid.clone());
        async move { draw(&gw, &sid, BOTH, Some(json!({"progressToken": "t-2"}))).await }
    });
    until("the job is polled", || async {
        sd.received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/sdcpp/v1/jobs/job_1")
            .count()
            >= 2
    })
    .await;
    let ack = gw
        .client()
        .post(format!("{gw}/mcp"))
        .header("accept", BOTH)
        .header("mcp-session-id", &sid)
        .json(
            &json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                      "params": {"requestId": 2, "reason": "the user pressed stop"}}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(ack.status().as_u16(), 202);
    let (_, body) = call.await.unwrap();
    let last = messages(&body).pop().expect("an answer");
    assert_eq!(last["result"]["isError"], json!(true), "{body}");
    let text = last["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Cancelled by the user"), "{text}");
    assert!(text.contains("lmgw stopped its container"), "{text}");
    until("the row's container is stopped", || async {
        podman.stopped().iter().any(|n| n.contains("-image-z-"))
    })
    .await;
}
