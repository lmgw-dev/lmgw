use super::*;
use lmgw_api_types::chat_media::{CancelKind, MediaPhase};

fn step(pass: u32, step: u32, total: u32) -> MediaProgress {
    MediaProgress::Generating {
        model: "image/z".into(),
        steps: Some(Steps { pass, step, total }),
        preview: None,
        cancel: CancelKind::Restart,
    }
}

#[test]
fn progress_only_rises_over_stages_steps_and_passes() {
    let mut c = Count::default();
    let stage = MediaProgress::Stage {
        phase: MediaPhase::Unloading,
        model: "chat/qwen".into(),
        room: None,
    };
    let a = c.next(&stage).unwrap();
    assert_eq!((a.0, a.2.as_str()), (0.5, "unloading chat/qwen"));
    let b = c
        .next(&MediaProgress::Stage {
            phase: MediaPhase::Loading,
            model: "image/z".into(),
            room: None,
        })
        .unwrap();
    assert_eq!(b.0, 0.75);
    let s = c.next(&step(1, 3, 8)).unwrap();
    assert_eq!(s, (3.0, Some(8), "image/z: step 3 / 8".to_string()));
    assert!(c.next(&step(1, 3, 8)).is_none(), "the same step again");
    // Pass 2 counts on from pass 1's eight steps, and has no total.
    let p2 = c.next(&step(2, 1, 4)).unwrap();
    assert_eq!(p2, (9.0, None, "image/z: step 1 / 4, pass 2".to_string()));
}

/// A cancel names a call of its own session: the same request id on
/// another session is another call.
#[test]
fn a_cancel_from_another_session_stops_nothing() {
    let (handle, stop) = crate::proxy::stop_pair();
    let id = json!(2);
    let _registered = Calls::register("session-a", &id, handle);
    let params = json!({"requestId": 2});
    assert!(!cancelled("session-b", &params));
    assert!(!stop.is_raised(), "another session's cancel");
    // `"2"` is another id than `2`.
    assert!(!cancelled("session-a", &json!({"requestId": "2"})));
    assert!(!stop.is_raised());
    assert!(cancelled("session-a", &params));
    assert!(stop.is_raised());
}

/// The dispatch tests' plane: `time__now` answers at once and reports no
/// progress; `image__wait` reports progress, and answers once its stop is
/// raised, saying so.
#[derive(Clone)]
struct Plane;

impl ToolPlane for Plane {
    fn list_tools<'a>(&'a self) -> crate::erased::Erased<'a, Vec<Value>> {
        Box::pin(async { Vec::new() })
    }

    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        _args: Option<Map<String, Value>>,
        cx: CallCx,
    ) -> crate::erased::Erased<'a, Result<Value, CallError>> {
        Box::pin(async move {
            if name == "image__wait" {
                crate::proxy::stopped(cx.stop.as_ref()).await;
            }
            Ok(json!({"content": [{"type": "text", "text": name}], "isError": false}))
        })
    }

    fn reports_progress(&self, name: &str) -> bool {
        name.starts_with("image__")
    }
}

const BOTH: &str = "application/json, text/event-stream";

fn headers(sid: Option<&str>) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("accept", BOTH.parse().unwrap());
    if let Some(sid) = sid {
        h.insert("mcp-session-id", sid.parse().unwrap());
    }
    h
}

async fn post(h: &HeaderMap, body: Value) -> Response {
    let body = axum::body::Bytes::from(serde_json::to_vec(&body).unwrap());
    super::super::dispatch(&Plane, h, &body, super::super::Plane::Aggregate).await
}

/// A new session's id.
async fn session() -> String {
    let resp = post(
        &headers(None),
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2025-11-25"}}),
    )
    .await;
    resp.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string()
}

/// `tools/call` of `name` as request `id` with a progress token.
fn call(id: i64, name: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
           "params": {"name": name, "_meta": {"progressToken": "t"}}})
}

fn content_type(resp: &Response) -> String {
    resp.headers()["content-type"].to_str().unwrap().to_string()
}

/// A tool that reports no progress answers one JSON body, token and
/// stream `Accept` or not; one that reports some is streamed.
#[tokio::test]
async fn only_a_tool_that_reports_progress_is_streamed() {
    let sid = session().await;
    let h = headers(Some(&sid));
    let resp = post(&h, call(2, "time__now")).await;
    assert!(
        content_type(&resp).starts_with("application/json"),
        "{}",
        content_type(&resp)
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["result"]["content"][0]["text"], json!("time__now"), "{v}");

    let resp = post(&h, call(3, "image__wait")).await;
    assert!(content_type(&resp).starts_with("text/event-stream"));
}

/// `notifications/cancelled` from another session leaves the call
/// running; from its own, it stops it.
#[tokio::test]
async fn a_cancelled_notification_from_another_session_leaves_the_call_running() {
    let (a, b) = (session().await, session().await);
    let running = tokio::spawn(async move {
        // Answered as JSON: no stream `Accept`.
        let mut h = HeaderMap::new();
        h.insert("accept", "application/json".parse().unwrap());
        h.insert("mcp-session-id", a.parse().unwrap());
        let resp = post(&h, call(7, "image__wait")).await;
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
    });
    let a = loop {
        let key = calls().keys().find(|(_, id)| id == "7").cloned();
        match key {
            Some((sid, _)) => break sid,
            None => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
        }
    };
    let cancel = json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                        "params": {"requestId": 7}});
    let ack = post(&headers(Some(&b)), cancel.clone()).await;
    assert_eq!(ack.status(), axum::http::StatusCode::ACCEPTED);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !running.is_finished(),
        "another session's cancel stopped it"
    );
    post(&headers(Some(&a)), cancel).await;
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(5), running)
        .await
        .expect("its own session's cancel stops it")
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["result"]["content"][0]["text"], json!("image__wait"));
}
