//! `mcp_call` items rendered back to the model (realtime-server-tools design
//! §2.6; WP3): a client's replayed calls — by the names their label was
//! listed under, a label the session never had by `<label>__<name>`, one
//! without a result with the synthetic one — and a tool's image result,
//! which the model gets as an image where its wire takes one, while the
//! item's `output` is text.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::mcp_stub::{answer, answering, echo_stub, register};
use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, next_event, send, user_text,
};
use crate::support::realtime_mcp::{calls, set_tools, shape, tools_session};

/// Replayed history renders: a call of a listed label by its exposed name
/// — after the label left the session too — one of a label the session
/// never had by `<label>__<name>`, its error as its result, and one with
/// neither output nor error with the synthetic result. Each has a call id
/// of its own, minted for it.
#[tokio::test]
async fn replayed_mcp_calls_render_by_the_names_their_label_was_listed_under() {
    let fake = chat_fake().await;
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let stub = echo_stub().await;
    register(&state, "alpha", "a", &stub.url, true, None).await;
    let mut ws = tools_session(
        &addr,
        None,
        json!([{"type": "mcp", "server_label": "a"}]),
        1,
    )
    .await;
    let create = |item: Value| json!({"type": "conversation.item.create", "item": item});
    for item in [
        json!({"type": "mcp_call", "server_label": "a", "name": "echo",
               "arguments": "{\"text\":\"r\"}", "output": "echo: r"}),
        json!({"type": "mcp_call", "server_label": "zz", "name": "do", "arguments": "{}",
               "error": {"type": "http_error", "code": 502, "message": "bad gateway"}}),
    ] {
        send(&mut ws, create(item)).await;
        events_until(&mut ws, "conversation.item.done").await;
    }
    // The label leaves; a call of it is history, and keeps its name.
    set_tools(&mut ws, json!([])).await;
    send(
        &mut ws,
        create(
            json!({"type": "mcp_call", "server_label": "a", "name": "echo",
                      "arguments": ""}),
        ),
    )
    .await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, user_text("and now?")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");

    let body = fake.seen.chat(0);
    let sent = shape(&body);
    assert_eq!(sent.len(), 6, "{sent:#?}");
    assert_eq!(
        sent[0].0, "user",
        "the conversation opens with the assistant"
    );
    let ids: Vec<&str> = body["messages"][1]["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(ids.iter().all(|i| i.starts_with("call_")), "{ids:?}");
    assert!(ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2]);
    let (a, z, pending) = (ids[0], ids[1], ids[2]);
    assert_eq!(
        sent[1..],
        [
            (
                "assistant".to_string(),
                format!(
                    r#"call:{a}:a__echo:{{"text":"r"}} | call:{z}:zz__do:{{}} | call:{pending}:a__echo:{{}}"#
                )
            ),
            ("tool".into(), format!("result:{a}:echo: r")),
            ("tool".into(), format!("result:{z}:bad gateway")),
            ("tool".into(), format!("result:{pending}:(no result yet)")),
            ("user".into(), "and now?".into()),
        ]
    );
    assert!(body.get("tools").is_none(), "the label left: {body}");
    assert!(stub.calls().is_empty(), "history is not run");
}

/// An Anthropic upstream as the alias `claude`, at `mock`.
async fn add_claude(state: &SharedState, mock: &MockServer) {
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "anthropic".into(),
            protocol: Protocol::Anthropic,
            kind: UpstreamKind::Generic,
            base_url: mock.uri(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "claude".into(),
            upstream_id: up,
            upstream_model_id: "claude-test".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

fn anthropic_sse(text: &str) -> String {
    format!(
        "event: message_start\ndata: {}\n\n\
         event: content_block_start\ndata: {}\n\n\
         event: content_block_delta\ndata: {}\n\n\
         event: message_delta\ndata: {}\n\n\
         event: message_stop\ndata: {}\n\n",
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 7}}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
               "usage": {"output_tokens": 3}}),
        json!({"type": "message_stop"}),
    )
}

/// A tool that answers with an image: the item's `output` is the result as
/// text, and the session keeps the image, so the next response — on an
/// upstream whose tool results take images — sends it as one (§2.4, §2.6).
#[tokio::test]
async fn an_image_result_reaches_the_model_on_the_follow_up() {
    let fake = chat_fake().await;
    fake.push(calls(&[(0, "call_i1", "a__shot", "{}")], "tool_calls"));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(anthropic_sse("A cat."), "text/event-stream"),
        )
        .mount(&mock)
        .await;
    add_claude(&state, &mock).await;
    let shot = answer(|_, _| async {
        json!({"content": [{"type": "text", "text": "a cat"},
                           {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}]})
    });
    let tools = json!([{"name": "shot", "inputSchema": {"type": "object"}}]);
    let stub = answering(tools, false, shot).await;
    register(&state, "alpha", "a", &stub.url, true, None).await;
    let mut ws = tools_session(
        &addr,
        None,
        json!([{"type": "mcp", "server_label": "a"}]),
        1,
    )
    .await;
    send(&mut ws, user_text("show me")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    let item = &events.last().unwrap()["response"]["output"][0];
    let output = item["output"].as_str().unwrap();
    assert!(
        output.starts_with("a cat\n") && output.contains("image/png image"),
        "{output}"
    );

    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime", "model": "claude"}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{events:#?}"
    );
    // The chat call, beside whatever else the route asked (a token count).
    let reqs = mock.received_requests().await.unwrap();
    let chats: Vec<_> = reqs
        .iter()
        .filter(|r| r.url.path().ends_with("/messages"))
        .collect();
    assert_eq!(
        chats.len(),
        1,
        "{:?}",
        reqs.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
    let body: Value = serde_json::from_slice(&chats[0].body).unwrap();
    let result = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .find(|c| c["type"] == "tool_result")
        .unwrap_or_else(|| panic!("no tool_result in {body:#}"));
    assert_eq!(result["tool_use_id"], "call_i1");
    let content = result["content"].as_array().unwrap();
    assert_eq!(content[0], json!({"type": "text", "text": "a cat"}));
    assert_eq!(content[1]["type"], "image", "{result}");
    assert_eq!(content[1]["source"]["data"], "aGVsbG8=");
}
