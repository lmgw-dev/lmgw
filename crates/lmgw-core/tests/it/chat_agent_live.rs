//! The tool loop streams live and leaves replayable records (chat-voice
//! design §7.2–§7.4): a tool thread's and a streamed `/v1/responses` run's
//! first token reaches the client while the model is still generating; a
//! Stop during a tool leaves a record in which every call has its result,
//! so the next send goes out well-formed; a turn that fails after its tools
//! ran keeps their record; and a failed send followed by another reaches
//! the model as one user message, on a plain thread and on a tool thread.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use lmgw_core::config::{Protocol, UpstreamKind};
use serde_json::{json, Value};
use tokio::sync::Notify;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_actions::{gateway, get_json, openai_sse, post, sse_events};
use crate::chat_golden::{mcp_stub, openai_call, read_until, register_stub, tool_thread};

/// An OpenAI-shaped upstream that streams `Hel`, then waits until `release`
/// is notified before it streams `lo` and ends: a model still generating
/// until the test has seen what it needs to see.
async fn held_upstream(release: Arc<Notify>) -> String {
    let handler = move || {
        let release = release.clone();
        async move {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
            tokio::spawn(async move {
                let chunk = |v: Value| Ok(Bytes::from(format!("data: {v}\n\n")));
                let _ = tx
                    .send(chunk(json!({"choices": [{"delta": {"content": "Hel"}}]})))
                    .await;
                release.notified().await;
                for v in [
                    json!({"choices": [{"delta": {"content": "lo"}}]}),
                    json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
                    json!({"choices": [], "usage": {"prompt_tokens": 4, "completion_tokens": 2}}),
                ] {
                    let _ = tx.send(chunk(v)).await;
                }
                let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
            });
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(
                    tokio_stream::wrappers::ReceiverStream::new(rx),
                ))
                .unwrap()
        }
    };
    let app = axum::Router::new().route("/chat/completions", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A gateway whose alias `m` answers from `base`.
pub(crate) async fn gateway_on(base: &str) -> (lmgw_core::state::SharedState, crate::common::Gw) {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let up = lmgw_core::store::insert_upstream(
        &state.db,
        &lmgw_core::store::NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: base.to_string(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    lmgw_core::store::insert_alias(
        &state.db,
        &lmgw_core::store::NewAlias {
            alias: "m".into(),
            upstream_id: up,
            upstream_model_id: "tgt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = crate::common::serve(state.clone()).await;
    (state, gw)
}

/// A tool thread's first `delta` reaches the page while the model is still
/// generating: the upstream does not finish until the test has seen it.
#[tokio::test]
async fn a_tool_threads_first_delta_arrives_before_the_model_finishes() {
    let release = Arc::new(Notify::new());
    let base = held_upstream(release.clone()).await;
    let (state, gw) = gateway_on(&base).await;
    register_stub(&state, &mcp_stub(Duration::ZERO).await).await;
    let tid = tool_thread(&gw).await;

    let mut resp = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi"}),
    )
    .await;
    let head = read_until(&mut resp, "event: delta").await;
    assert!(head.contains("\"text\":\"Hel\""), "{head}");
    assert!(!head.contains("event: done"), "{head}");
    release.notify_one();
    let body = format!("{head}{}", resp.text().await.unwrap());
    let events = sse_events(&body);
    let done = &events.last().unwrap().1;
    assert_eq!(done["saved"], true, "{body}");
    assert_eq!(done["aborted"], false, "{body}");
    let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(detail["messages"][1]["content"], "Hello", "{detail}");
}

/// The same for a streamed `/v1/responses` run, which shares the loop.
#[tokio::test]
async fn a_streamed_response_relays_text_before_the_model_finishes() {
    let release = Arc::new(Notify::new());
    let base = held_upstream(release.clone()).await;
    let (_state, gw) = gateway_on(&base).await;

    let mut resp = reqwest::Client::new()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": "m", "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let head = read_until(&mut resp, "response.output_text.delta").await;
    assert!(head.contains("\"delta\":\"Hel\""), "{head}");
    assert!(!head.contains("response.completed"), "{head}");
    release.notify_one();
    let rest = resp.text().await.unwrap();
    assert!(rest.contains("response.completed"), "{rest}");
}

/// Every assistant tool call in a request has its tool result right after
/// it — what a strict upstream checks before answering.
fn assert_well_formed(req: &Value) {
    let msgs = req["messages"].as_array().unwrap();
    for (i, m) in msgs.iter().enumerate() {
        let Some(calls) = m["tool_calls"].as_array() else {
            continue;
        };
        for call in calls {
            let id = call["id"].as_str().unwrap();
            let answered = msgs[i + 1..]
                .iter()
                .take_while(|n| n["role"] == "tool")
                .any(|n| n["tool_call_id"] == id);
            assert!(answered, "call {id} has no result: {req:#}");
        }
    }
}

/// Stop while a tool is running, then send again: the stopped turn's record
/// gives the abandoned call its result, so the next request is one a strict
/// upstream answers.
#[tokio::test]
async fn stop_during_a_slow_tool_then_send_again_succeeds() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("take your time"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            openai_call("Looking. ", "c1", "stub__slow", "{\"text\":\"zzz\"}"),
            "text/event-stream",
        ))
        .up_to_n_times(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("never mind"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(openai_sse("fine", 3, 1), "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    register_stub(&state, &mcp_stub(Duration::from_secs(30)).await).await;
    let tid = tool_thread(&gw).await;

    let mut first = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "take your time"}),
    )
    .await;
    read_until(&mut first, "\"event\":\"ready\"").await;
    // Stop: the page drops the stream while the tool runs.
    drop(first);

    // The stopped turn saves what it had once its cancel lands.
    let mut saved = Value::Null;
    for _ in 0..100 {
        let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
        if detail["messages"].as_array().unwrap().len() == 2 {
            saved = detail["messages"][1].clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(saved["content"], "Looking. ", "{saved}");
    let record: Value = serde_json::from_str(saved["ir_messages"].as_str().unwrap()).unwrap();
    assert_eq!(record[1]["role"], "tool", "{record:#}");
    assert!(
        record[1]
            .to_string()
            .contains("abandoned when the run was cancelled"),
        "{record:#}"
    );

    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "never mind"}),
    )
    .await
    .text()
    .await
    .unwrap();
    let events = sse_events(&body);
    let done = &events.last().unwrap().1;
    assert_eq!(done["saved"], true, "{body}");
    assert_eq!(done["aborted"], false, "{body}");

    let reqs = mock.received_requests().await.unwrap();
    let last: Value = serde_json::from_slice(&reqs.last().unwrap().body).unwrap();
    assert!(last.to_string().contains("never mind"));
    assert_well_formed(&last);
}

/// A turn the tool-call budget stopped before its calls were made stores a
/// result for each of them, so the thread stays answerable.
#[tokio::test]
async fn a_turn_out_of_budget_records_its_unmade_calls() {
    let mock = MockServer::start().await;
    let two_calls = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": "Both. ", "tool_calls": [
            {"index": 0, "id": "c1", "type": "function",
             "function": {"name": "stub__echo", "arguments": "{}"}}]}}]}),
        json!({"choices": [{"delta": {"tool_calls": [
            {"index": 1, "id": "c2", "type": "function",
             "function": {"name": "stub__echo", "arguments": "{}"}}]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(two_calls, "text/event-stream"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(openai_sse("ok", 3, 1), "text/event-stream"),
        )
        .with_priority(2)
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let mut settings = state.snapshot().settings.clone();
    settings.responses_max_tool_calls = 1;
    lmgw_core::store::save_settings(&state.db, &settings)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    register_stub(&state, &mcp_stub(Duration::ZERO).await).await;
    let tid = tool_thread(&gw).await;

    for q in ["echo twice", "and now?"] {
        let body = post(
            &gw,
            &format!("/chat/api/threads/{tid}/send"),
            json!({"content": q}),
        )
        .await
        .text()
        .await
        .unwrap();
        assert!(body.contains("event: done"), "{body}");
    }
    let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    let record: Value =
        serde_json::from_str(detail["messages"][1]["ir_messages"].as_str().unwrap()).unwrap();
    assert_eq!(record[1]["role"], "tool", "{record:#}");
    assert!(record[1].to_string().contains("not run"), "{record:#}");

    let reqs = mock.received_requests().await.unwrap();
    let last: Value = serde_json::from_slice(&reqs.last().unwrap().body).unwrap();
    assert!(last.to_string().contains("and now?"));
    assert_well_formed(&last);
}

/// A turn whose second model call fails after its tool ran keeps the tool's
/// record (WP2 review M3): the model learns on the next send that the call
/// happened, rather than making it again.
#[tokio::test]
async fn a_turn_failing_after_its_tools_ran_keeps_their_record() {
    let mock = MockServer::start().await;
    let replies = [
        ResponseTemplate::new(200).set_body_raw(
            openai_call("Checking. ", "c1", "stub__echo", "{\"text\":\"hi\"}"),
            "text/event-stream",
        ),
        ResponseTemplate::new(500)
            .set_body_json(json!({"error": {"message": "the model fell over"}})),
        ResponseTemplate::new(200).set_body_raw(openai_sse("it ran", 5, 2), "text/event-stream"),
    ];
    for (i, reply) in replies.into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(reply)
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .mount(&mock)
            .await;
    }
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    register_stub(&state, &mcp_stub(Duration::ZERO).await).await;
    let tid = tool_thread(&gw).await;

    let body = send_text(&gw, tid, "echo hi").await;
    let events = sse_events(&body);
    assert!(
        events
            .iter()
            .any(|(e, d)| e == "error" && d["message"].as_str().unwrap().contains("fell over")),
        "{body}"
    );
    let done = &events.last().unwrap().1;
    assert_eq!(done["aborted"], true, "{body}");
    assert_eq!(done["saved"], true, "the record is saved: {body}");
    let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    let reply = &detail["messages"][1];
    assert_eq!(reply["content"], "Checking. ", "{reply}");
    let record: Value = serde_json::from_str(reply["ir_messages"].as_str().unwrap()).unwrap();
    assert_eq!(record[0]["role"], "assistant", "{record:#}");
    assert_eq!(record[1]["role"], "tool", "{record:#}");
    assert!(record[1].to_string().contains("echoed"), "{record:#}");

    // The next send replays the call and its real result.
    send_text(&gw, tid, "did it work?").await;
    let reqs = mock.received_requests().await.unwrap();
    let last: Value = serde_json::from_slice(&reqs.last().unwrap().body).unwrap();
    assert!(last.to_string().contains("did it work?"));
    assert_well_formed(&last);
    let tool = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("the tool's result is replayed");
    assert!(tool.to_string().contains("echoed"), "{last:#}");
}

async fn send_text(gw: &crate::common::Gw, tid: i64, q: &str) -> String {
    post(
        gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": q}),
    )
    .await
    .text()
    .await
    .unwrap()
}

/// A send the upstream refused keeps its user message and saves no reply —
/// on a tool thread as on a plain one (WP2 review M2) — so the next send
/// must not put two user messages in a row on the wire.
async fn a_failed_send_then_a_send(tools: bool) {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_json(json!({"error": {"message": "the model fell over"}})),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(openai_sse("got both", 5, 2), "text/event-stream"),
        )
        .with_priority(2)
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = if tools {
        register_stub(&state, &mcp_stub(Duration::ZERO).await).await;
        tool_thread(&gw).await
    } else {
        post(&gw, "/chat/api/threads", json!({"model_alias": "m"}))
            .await
            .json::<Value>()
            .await
            .unwrap()["id"]
            .as_i64()
            .unwrap()
    };
    let failed = send_text(&gw, tid, "first try").await;
    assert_eq!(
        sse_events(&failed).last().unwrap(),
        &("done".to_string(), json!({"aborted": true})),
        "nothing saved, and said so: {failed}"
    );
    let body = send_text(&gw, tid, "second try").await;
    assert!(body.contains("event: done"), "{body}");

    let reqs = mock.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    let second: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    let users: Vec<&Value> = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .collect();
    assert_eq!(users.len(), 1, "{second:#}");
    assert_eq!(users[0]["content"], "first try\n\nsecond try", "{second:#}");
    // What is stored is untouched: two user messages, then the reply.
    let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    let roles: Vec<&str> = detail["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "user", "assistant"]);
}

#[tokio::test]
async fn a_failed_send_then_a_send_reach_the_model_as_one_user_message() {
    a_failed_send_then_a_send(false).await;
}

#[tokio::test]
async fn on_a_tool_thread_a_failed_send_then_a_send_reach_the_model_as_one_user_message() {
    a_failed_send_then_a_send(true).await;
}
