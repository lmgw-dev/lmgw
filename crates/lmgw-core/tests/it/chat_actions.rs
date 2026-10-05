//! Chat message actions (chat-complete design §3): delete, edit, regenerate,
//! continue — thread-scoped, final (what they cut is gone), and answered by
//! the same turn a send starts.
//!
//! Also the fixture `chat_temporary` shares: a gateway with one alias `m` on a
//! wiremock upstream of a chosen kind and protocol.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A gateway whose alias `m` routes to `mock` as an upstream of `kind` and
/// `protocol`.
pub(crate) async fn gateway(
    mock: &MockServer,
    kind: UpstreamKind,
    protocol: Protocol,
) -> (SharedState, Gw) {
    gateway_at(&mock.uri(), kind, protocol).await
}

/// [`gateway`] on an upstream at `base` that is not a wiremock server.
pub(crate) async fn gateway_at(
    base: &str,
    kind: UpstreamKind,
    protocol: Protocol,
) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol,
            kind,
            base_url: base.to_string(),
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
            alias: "m".into(),
            upstream_id: up_id,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

/// An OpenAI-shaped streamed answer `text` (in two chunks) with usage.
pub(crate) fn openai_sse(text: &str, prompt_tokens: u32, completion_tokens: u32) -> String {
    let (a, b) = text.split_at(text.len() / 2);
    let chunk = |t: &str| {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"content": t}}]})
        )
    };
    format!(
        "{}{}data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(a),
        chunk(b),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": prompt_tokens,
                                        "completion_tokens": completion_tokens}}),
    )
}

/// Answer every `/chat/completions` with [`openai_sse`].
pub(crate) async fn mount_openai_reply(
    mock: &MockServer,
    text: &str,
    prompt_tokens: u32,
    completion_tokens: u32,
) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    openai_sse(text, prompt_tokens, completion_tokens),
                    "text/event-stream",
                ),
        )
        .mount(mock)
        .await;
}

pub(crate) async fn post(gw: &Gw, route: &str, body: Value) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}{route}"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

pub(crate) async fn get_json(gw: &Gw, route: &str) -> Value {
    let r = gw
        .client()
        .get(format!("{gw}{route}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "GET {route}");
    r.json().await.unwrap()
}

/// A Chat SSE body as `(event, data)` pairs, in order.
pub(crate) fn sse_events(body: &str) -> Vec<(String, Value)> {
    body.split("\n\n")
        .filter_map(|block| {
            let mut event = None;
            let mut data = String::new();
            for line in block.lines() {
                if let Some(e) = line.strip_prefix("event:") {
                    event = Some(e.trim().to_string());
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push_str(d.trim_start());
                }
            }
            Some((event?, serde_json::from_str(&data).unwrap_or(Value::Null)))
        })
        .collect()
}

/// A thread on alias `m`, created through the API (so it carries the default
/// system prompt, as any Chat thread does).
async fn thread(gw: &Gw) -> i64 {
    post(gw, "/chat/api/threads", json!({"model_alias": "m"}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}

/// Write a message straight into a stored thread — history to act on
/// without a mock answering every turn that built it.
async fn seed(state: &SharedState, tid: i64, role: &str, content: &str) -> i64 {
    store::append_chat_message(&state.db, tid, role, content, "", None, None, None)
        .await
        .unwrap()
}

async fn messages(gw: &Gw, tid: i64) -> Vec<Value> {
    get_json(gw, &format!("/chat/api/threads/{tid}")).await["messages"]
        .as_array()
        .unwrap()
        .clone()
}

/// The body of the last request the upstream saw.
async fn last_upstream_body(mock: &MockServer) -> Value {
    let seen = mock.received_requests().await.unwrap();
    serde_json::from_slice(&seen.last().expect("the upstream was called").body).unwrap()
}

/// The request's turns past the system prompt, as `(role, content)`.
fn turns(body: &Value) -> Vec<(String, String)> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] != "system")
        .map(|m| {
            let content = match &m["content"] {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("|"),
                other => other.to_string(),
            };
            (m["role"].as_str().unwrap().to_string(), content)
        })
        .collect()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(r, c)| (r.to_string(), c.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_removes_one_message_with_its_attachments_and_nothing_else() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "noted", 3, 1).await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw).await;
    let att: Value = gw
        .client()
        .post(format!(
            "{gw}/chat/api/threads/{tid}/attachments?name=a.txt"
        ))
        .body("file text")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();
    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "one", "attachments": [aid]}),
    )
    .await
    .text()
    .await
    .unwrap();
    let events = sse_events(&body);
    assert_eq!(events[0].0, "turn", "{body}");
    let user_id = events[0].1["user_message_id"].as_i64().unwrap();
    let after = seed(&state, tid, "user", "two").await;

    // Not this thread's message: 404, and nothing goes.
    let other = thread(&gw).await;
    let r = post(
        &gw,
        &format!("/chat/api/threads/{other}/messages/{user_id}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(r.status(), 404);

    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{user_id}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let msgs = messages(&gw, tid).await;
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["assistant", "user"], "only the one message goes");
    assert_eq!(msgs[1]["id"], after);
    let r = gw
        .client()
        .get(format!("{gw}/chat/api/attachments/{aid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "its attachment went with it");

    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{user_id}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(r.status(), 404, "a second delete finds nothing");
}

// ---------------------------------------------------------------------------
// Edit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn editing_a_reply_rewrites_it_in_place_and_drops_what_described_the_old_text() {
    let mock = MockServer::start().await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw).await;
    seed(&state, tid, "user", "q").await;
    let rid = store::append_chat_message(
        &state.db,
        tid,
        "assistant",
        "old answer",
        "old thoughts",
        Some(10),
        Some(4),
        Some(r#"[{"role":"assistant","content":[]}]"#),
    )
    .await
    .unwrap();
    let later = seed(&state, tid, "user", "and then?").await;

    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{rid}/edit"),
        json!({"content": "better answer"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["message"]["id"], rid, "{v}");
    assert_eq!(v["message"]["content"], "better answer");
    assert_eq!(v["message"]["reasoning"], "");
    assert!(v["message"]["prompt_tokens"].is_null(), "{v}");
    assert!(v["message"]["completion_tokens"].is_null(), "{v}");
    assert!(v["message"]["ir_messages"].is_null(), "{v}");

    let msgs = messages(&gw, tid).await;
    assert_eq!(msgs.len(), 3, "nothing after it goes");
    assert_eq!(msgs[1]["content"], "better answer");
    assert_eq!(msgs[2]["id"], later);
    assert!(
        mock.received_requests().await.unwrap().is_empty(),
        "an edited reply is not resent"
    );
}

#[tokio::test]
async fn editing_a_user_message_cuts_everything_after_it_and_answers_it_again() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "fresh answer", 6, 2).await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw).await;
    let att: Value = gw
        .client()
        .post(format!(
            "{gw}/chat/api/threads/{tid}/attachments?name=ctx.txt"
        ))
        .body("attached context")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();
    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "first question", "attachments": [aid]}),
    )
    .await
    .text()
    .await
    .unwrap();
    let q1 = sse_events(&body)[0].1["user_message_id"].as_i64().unwrap();
    seed(&state, tid, "user", "second question").await;
    seed(&state, tid, "assistant", "second answer").await;

    // Empty text on a message that has files is fine; with none it is not.
    let bare = seed(&state, tid, "user", "no files here").await;
    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{bare}/edit"),
        json!({"content": "  "}),
    )
    .await;
    assert_eq!(r.status(), 400);
    assert_eq!(r.json::<Value>().await.unwrap()["code"], "empty_message");

    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{q1}/edit"),
        json!({"content": "  the better question "}),
    )
    .await
    .text()
    .await
    .unwrap();
    let events = sse_events(&body);
    assert_eq!(events[0].0, "turn", "{body}");
    assert_eq!(events[0].1["user_message_id"], q1, "{body}");
    let done = &events.last().unwrap().1;
    assert_eq!(events.last().unwrap().0, "done", "{body}");

    let msgs = messages(&gw, tid).await;
    assert_eq!(msgs.len(), 2, "everything after the edited message went");
    assert_eq!(msgs[0]["id"], q1, "edited in place, same row");
    assert_eq!(msgs[0]["content"], "the better question");
    assert_eq!(
        msgs[0]["attachments"][0]["id"], aid,
        "its attachment stays bound"
    );
    assert_eq!(msgs[1]["content"], "fresh answer");
    assert_eq!(msgs[1]["id"], done["message_id"]);

    // The model was asked the edited question alone, file included.
    let sent = last_upstream_body(&mock).await;
    let asked = turns(&sent);
    assert_eq!(asked.len(), 1, "{sent}");
    assert_eq!(asked[0].0, "user");
    assert!(asked[0].1.contains("attached context"), "{sent}");
    assert!(asked[0].1.ends_with("the better question"), "{sent}");
}

// ---------------------------------------------------------------------------
// Regenerate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn regenerating_a_reply_answers_the_history_before_it() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "another take", 4, 2).await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw).await;
    let q1 = seed(&state, tid, "user", "q1").await;
    let a1 = seed(&state, tid, "assistant", "a1").await;
    seed(&state, tid, "user", "q2").await;
    seed(&state, tid, "assistant", "a2").await;

    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{a1}/regenerate"),
        json!({}),
    )
    .await
    .text()
    .await
    .unwrap();
    let events = sse_events(&body);
    assert!(
        events.iter().all(|(e, _)| e != "turn"),
        "no user message was written: {body}"
    );
    let msgs = messages(&gw, tid).await;
    assert_eq!(msgs.len(), 2, "the reply and everything after it went");
    assert_eq!(msgs[0]["id"], q1);
    assert_eq!(msgs[1]["content"], "another take");
    assert_eq!(
        turns(&last_upstream_body(&mock).await),
        pairs(&[("user", "q1")])
    );
}

#[tokio::test]
async fn regenerating_a_user_message_answers_it_again() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "retry", 4, 1).await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw).await;
    seed(&state, tid, "user", "q1").await;
    seed(&state, tid, "assistant", "a1").await;
    let q2 = seed(&state, tid, "user", "q2").await;
    seed(&state, tid, "assistant", "a2").await;
    seed(&state, tid, "user", "q3").await;

    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{q2}/regenerate"),
        json!({}),
    )
    .await
    .text()
    .await
    .unwrap();
    let events = sse_events(&body);
    assert_eq!(events[0].0, "turn", "{body}");
    assert_eq!(events[0].1["user_message_id"], q2);
    let msgs = messages(&gw, tid).await;
    let contents: Vec<&str> = msgs
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents, ["q1", "a1", "q2", "retry"]);
    assert_eq!(
        turns(&last_upstream_body(&mock).await),
        pairs(&[("user", "q1"), ("assistant", "a1"), ("user", "q2")])
    );
}

#[tokio::test]
async fn a_reply_with_no_user_message_before_it_cannot_be_regenerated() {
    let mock = MockServer::start().await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw).await;
    let lone = seed(&state, tid, "assistant", "hello first").await;
    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{lone}/regenerate"),
        json!({}),
    )
    .await;
    assert_eq!(r.status(), 409);
    assert_eq!(
        r.json::<Value>().await.unwrap()["code"],
        "nothing_to_answer"
    );
    assert_eq!(messages(&gw, tid).await.len(), 1, "nothing was deleted");

    // And a message of another thread is not this thread's to act on.
    let other = thread(&gw).await;
    for action in ["regenerate", "edit"] {
        let r = post(
            &gw,
            &format!("/chat/api/threads/{other}/messages/{lone}/{action}"),
            json!({"content": "x"}),
        )
        .await;
        assert_eq!(r.status(), 404, "{action}");
    }
}

// ---------------------------------------------------------------------------
// Continue
// ---------------------------------------------------------------------------

async fn continue_state(gw: &Gw, tid: i64) -> Value {
    get_json(gw, &format!("/chat/api/threads/{tid}")).await["thread"]["continue"].clone()
}

#[tokio::test]
async fn continue_on_llama_server_prefills_the_reply_and_appends_to_it() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, " there lived a cat.", 12, 5).await;
    let (state, gw) = gateway(&mock, UpstreamKind::LlamaServer, Protocol::Openai).await;
    let tid = thread(&gw).await;
    seed(&state, tid, "user", "tell a story").await;
    let rid = store::append_chat_message(
        &state.db,
        tid,
        "assistant",
        "Once upon a time \n",
        "a story it is",
        Some(8),
        Some(3),
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        continue_state(&gw, tid).await,
        json!({"ok": true, "reason": null})
    );
    let body = post(&gw, &format!("/chat/api/threads/{tid}/continue"), json!({}))
        .await
        .text()
        .await
        .unwrap();
    let events = sse_events(&body);
    assert!(events.iter().all(|(e, _)| e != "turn"), "{body}");
    let deltas: String = events
        .iter()
        .filter(|(e, _)| e == "delta")
        .map(|(_, d)| d["text"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, " there lived a cat.", "the continuation only");
    let (last, done) = events.last().unwrap();
    assert_eq!(last, "done");
    assert_eq!(done["message_id"], rid, "{body}");

    // The reply went out last, as the prefill, its trailing whitespace off,
    // its trace with it.
    let sent = last_upstream_body(&mock).await;
    let out = sent["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(out["role"], "assistant", "{sent}");
    assert_eq!(out["content"], "Once upon a time", "{sent}");
    assert_eq!(out["reasoning_content"], "a story it is", "{sent}");
    // Said explicitly, so llama-server's parser knows the answer began
    // before the generation and does not stream the prefill back.
    assert_eq!(sent["continue_final_message"], true, "{sent}");
    assert_eq!(sent["add_generation_prompt"], false, "{sent}");

    let msgs = messages(&gw, tid).await;
    assert_eq!(msgs.len(), 2, "appended, not a new row");
    assert_eq!(msgs[1]["id"], rid);
    assert_eq!(msgs[1]["content"], "Once upon a time there lived a cat.");
    assert_eq!(msgs[1]["reasoning"], "a story it is");
    assert_eq!(msgs[1]["prompt_tokens"], 12, "the last call's counts");
    assert_eq!(msgs[1]["completion_tokens"], 5);

    // An ordinary send to the same route is no continuation.
    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "another"}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert!(body.contains("event: done"), "{body}");
    let sent = last_upstream_body(&mock).await;
    assert!(sent.get("continue_final_message").is_none(), "{sent}");
    assert!(sent.get("add_generation_prompt").is_none(), "{sent}");
}

#[tokio::test]
async fn continue_on_anthropic_prefills_the_trailing_assistant_turn() {
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" and 3.\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse, "text/event-stream"),
        )
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Anthropic).await;
    let tid = thread(&gw).await;
    seed(&state, tid, "user", "count to 3").await;
    let rid = seed(&state, tid, "assistant", "1, 2 ").await;

    assert_eq!(continue_state(&gw, tid).await["ok"], true);
    let body = post(&gw, &format!("/chat/api/threads/{tid}/continue"), json!({}))
        .await
        .text()
        .await
        .unwrap();
    assert!(body.contains("event: done"), "{body}");

    let sent: Value = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v1/messages")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .next_back()
        .expect("a messages call");
    let last = sent["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "assistant", "{sent}");
    assert_eq!(
        last["content"],
        json!([{"type": "text", "text": "1, 2"}]),
        "no trailing whitespace: Anthropic refuses it on a prefill — {sent}"
    );
    assert!(
        sent.get("continue_final_message").is_none(),
        "llama-server's fields stay off Anthropic: {sent}"
    );

    let msgs = messages(&gw, tid).await;
    assert_eq!(msgs[1]["id"], rid);
    assert_eq!(msgs[1]["content"], "1, 2 and 3.");
    assert_eq!(msgs[1]["completion_tokens"], 3);
}

#[tokio::test]
async fn continue_is_refused_where_there_is_no_prefill_or_nothing_to_continue() {
    // An OpenAI-compatible cloud route has no prefill.
    let mock = MockServer::start().await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw).await;
    let st = continue_state(&gw, tid).await;
    assert_eq!(st["ok"], false);
    assert!(st["reason"].as_str().unwrap().contains("no reply"), "{st}");
    seed(&state, tid, "user", "q").await;
    let st = continue_state(&gw, tid).await;
    assert!(
        st["reason"].as_str().unwrap().contains("not a reply"),
        "{st}"
    );
    seed(&state, tid, "assistant", "a").await;
    let st = continue_state(&gw, tid).await;
    assert_eq!(st["ok"], false);
    let why = st["reason"].as_str().unwrap();
    assert!(why.contains("OpenAI-compatible"), "{why}");
    let r = post(&gw, &format!("/chat/api/threads/{tid}/continue"), json!({})).await;
    assert_eq!(r.status(), 409);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "continue_unavailable");
    assert_eq!(v["message"], why, "the same reason the thread shows");
    assert!(mock.received_requests().await.unwrap().is_empty());

    // Anthropic: fine, until the thread switches extended thinking on — the
    // settings answer says so at once.
    let mock = MockServer::start().await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Anthropic).await;
    let tid = thread(&gw).await;
    seed(&state, tid, "user", "q").await;
    seed(&state, tid, "assistant", "a").await;
    assert_eq!(continue_state(&gw, tid).await["ok"], true);
    let v: Value = post(
        &gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"reasoning_enabled": true}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(v["continue"]["ok"], false, "{v}");
    assert!(
        v["continue"]["reason"]
            .as_str()
            .unwrap()
            .contains("extended thinking"),
        "{v}"
    );

    // A reply that ran tools carries its record, and cannot be continued; an
    // empty record is no record.
    let mock = MockServer::start().await;
    let (state, gw) = gateway(&mock, UpstreamKind::LlamaServer, Protocol::Openai).await;
    let tid = thread(&gw).await;
    seed(&state, tid, "user", "q").await;
    let with_tools = store::append_chat_message(
        &state.db,
        tid,
        "assistant",
        "done",
        "",
        None,
        None,
        Some(r#"[{"role":"assistant","content":[{"type":"text","text":"x"}]}]"#),
    )
    .await
    .unwrap();
    let st = continue_state(&gw, tid).await;
    assert!(
        st["reason"].as_str().unwrap().contains("tool record"),
        "{st}"
    );
    post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{with_tools}/delete"),
        json!({}),
    )
    .await;
    store::append_chat_message(&state.db, tid, "assistant", "a", "", None, None, Some("[]"))
        .await
        .unwrap();
    assert_eq!(continue_state(&gw, tid).await["ok"], true);
}

/// The thread's own route takes a prefill, but the GPU hold sends the turn to
/// a cloud fallback that does not: the continue is refused in the stream, by
/// name, and the reply is left as it was — never answered with a fresh
/// message appended to it.
#[tokio::test]
async fn a_hold_reroute_to_a_route_without_prefill_refuses_the_continue() {
    use crate::support::gpu_world::{Gpu, GIB};
    use lmgw_core::config::HoldFallbackMode;
    use lmgw_core::store::NewLocalModel;

    let g = Gpu::new(24 * GIB, 1, 2).await;
    g.cloud("cloud", None).await;
    g.row(
        NewLocalModel {
            model_id: "p".into(),
            gguf_path: "p.gguf".into(),
            params: Default::default(),
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: HoldFallbackMode::Alias,
            hold_fallback: Some("cloud".into()),
            capabilities_override: None,
            ladder: vec![],
        },
        8 * GIB,
    )
    .await;
    let gw = serve(g.state.clone()).await;
    let tid = post(&gw, "/chat/api/threads", json!({"model_alias": "p"}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    seed(&g.state, tid, "user", "q").await;
    let rid = seed(&g.state, tid, "assistant", "half an answer").await;
    assert_eq!(continue_state(&gw, tid).await["ok"], true);

    let mut s = g.state.snapshot().settings.clone();
    s.hold.active = true;
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();

    let body = post(&gw, &format!("/chat/api/threads/{tid}/continue"), json!({}))
        .await
        .text()
        .await
        .unwrap();
    let events = sse_events(&body);
    let error = events
        .iter()
        .find(|(e, _)| e == "error")
        .unwrap_or_else(|| panic!("no error event: {body}"));
    let msg = error.1["message"].as_str().unwrap();
    assert!(msg.contains("cannot continue this reply"), "{msg}");
    assert!(msg.contains("OpenAI-compatible"), "{msg}");
    assert_eq!(events.last().unwrap().1["aborted"], true, "{body}");

    let msgs = messages(&gw, tid).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1]["id"], rid);
    assert_eq!(msgs[1]["content"], "half an answer", "left as it was");
    assert!(g.runs().is_empty(), "nothing local starts under the hold");
}

/// A thread with tools continues through the tool loop: its first request
/// carries the tools *and* the explicit continuation, and the reply grows in
/// place like a plain one.
#[tokio::test]
async fn continue_through_the_tool_loop_appends_to_the_reply() {
    use lmgw_core::config::{SelfAdmin, Settings};

    let mock = MockServer::start().await;
    mount_openai_reply(&mock, " the end.", 30, 3).await;
    let (state, gw) = gateway(&mock, UpstreamKind::LlamaServer, Protocol::Openai).await;
    let s = Settings {
        self_admin: SelfAdmin::ReadOnly,
        ..state.snapshot().settings.clone()
    };
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let tid = post(
        &gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "kind": "admin"}),
    )
    .await
    .json::<Value>()
    .await
    .unwrap()["id"]
        .as_i64()
        .unwrap();
    seed(&state, tid, "user", "summarise").await;
    let rid = seed(&state, tid, "assistant", "In short,").await;

    let body = post(&gw, &format!("/chat/api/threads/{tid}/continue"), json!({}))
        .await
        .text()
        .await
        .unwrap();
    let events = sse_events(&body);
    assert_eq!(events.last().unwrap().1["message_id"], rid, "{body}");

    let sent = last_upstream_body(&mock).await;
    assert!(sent["tools"].is_array(), "the loop's tools: {sent}");
    assert_eq!(sent["continue_final_message"], true, "{sent}");
    let last = sent["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "assistant");
    assert_eq!(last["content"], "In short,");

    let msgs = messages(&gw, tid).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1]["content"], "In short, the end.");
    assert!(msgs[1]["ir_messages"].is_null(), "no tool ran: {msgs:?}");
    assert_eq!(msgs[1]["completion_tokens"], 3);
}

/// A continue whose continuation calls a tool (review R1 finding 4): the
/// record's first reply carries the prefill, so the row's text is the record's
/// text plus the final answer — and the next turn replays the reply once, in
/// order, with its tool call and result.
#[tokio::test]
async fn a_continue_that_calls_a_tool_replays_without_duplication() {
    use lmgw_core::config::{SelfAdmin, Settings};
    use wiremock::matchers::body_string_contains;

    let call = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\" let me check.\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
        "\"type\":\"function\",\"function\":{\"name\":\"lmgw__status\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(call, "text/event-stream"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&mock)
        .await;
    // The answer after the tool, and later the next turn's.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("lmgw__status"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(openai_sse(" All good.", 40, 3), "text/event-stream"),
        )
        .with_priority(2)
        .mount(&mock)
        .await;
    let (state, gw) = gateway(&mock, UpstreamKind::LlamaServer, Protocol::Openai).await;
    let s = Settings {
        self_admin: SelfAdmin::ReadOnly,
        ..state.snapshot().settings.clone()
    };
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let tid = post(
        &gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "kind": "admin"}),
    )
    .await
    .json::<Value>()
    .await
    .unwrap()["id"]
        .as_i64()
        .unwrap();
    seed(&state, tid, "user", "summarise").await;
    let rid = seed(&state, tid, "assistant", "In short,").await;

    let body = post(&gw, &format!("/chat/api/threads/{tid}/continue"), json!({}))
        .await
        .text()
        .await
        .unwrap();
    assert_eq!(
        sse_events(&body).last().unwrap().1["message_id"],
        rid,
        "{body}"
    );

    let msgs = messages(&gw, tid).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1]["content"], "In short, let me check. All good.");
    let record: Value = serde_json::from_str(msgs[1]["ir_messages"].as_str().unwrap()).unwrap();
    let first = &record[0];
    assert_eq!(first["role"], "assistant", "{record}");
    let first_text: String = first["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["text"].as_str())
        .collect();
    assert_eq!(first_text, "In short, let me check.", "{record}");

    // The next turn replays it once: the prefill and what followed it, the
    // tool call and its result, then only the final answer.
    let next = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "thanks"}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert_eq!(
        sse_events(&next).last().unwrap().1["aborted"],
        false,
        "{next}"
    );
    let sent = last_upstream_body(&mock).await;
    let said: Vec<String> = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .filter_map(|m| m["content"].as_str().map(str::to_string))
        .collect();
    assert_eq!(said.concat(), "In short, let me check. All good.", "{sent}");
    let roles: Vec<&str> = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .filter(|r| *r != "system")
        .collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "tool", "assistant", "user"],
        "{sent}"
    );
}

/// A thread on a candidate alias can be continued (review R1 finding 3):
/// `Snapshot::resolve` refuses a candidate alias by design, and the verdict
/// used to repeat that refusal. Every candidate is a local chat model on
/// llama-server, which takes the prefill.
#[tokio::test]
async fn a_candidate_alias_thread_can_be_continued() {
    use lmgw_core::config::HoldFallbackMode;
    use lmgw_core::store::NewLocalModel;

    let state = AppState::init_for_tests().await.unwrap();
    for id in ["c1", "c2"] {
        store::insert_local_model(
            &state.db,
            &NewLocalModel {
                model_id: id.into(),
                gguf_path: format!("{id}.gguf"),
                params: Default::default(),
                args: vec![],
                idle_seconds: 0,
                enabled: true,
                public: true,
                image: None,
                extra_run_args: None,
                warm_start: false,
                hold_fallback_mode: HoldFallbackMode::Inherit,
                hold_fallback: None,
                capabilities_override: None,
                ladder: vec![],
            },
        )
        .await
        .unwrap();
    }
    store::insert_candidate_alias(
        &state.db,
        &store::NewCandidateAlias {
            alias: "pick".into(),
            candidates: vec!["c1".into(), "c2".into()],
            background: false,
            fallback_mode: HoldFallbackMode::Inherit,
            fallback: None,
            capabilities_disabled: vec![],
            capabilities_enabled: vec![],
            enabled: true,
            notes: String::new(),
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    let tid = post(&gw, "/chat/api/threads", json!({"model_alias": "pick"}))
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    seed(&state, tid, "user", "q").await;
    seed(&state, tid, "assistant", "half").await;
    let verdict = continue_state(&gw, tid).await;
    assert_eq!(verdict["ok"], true, "{verdict}");
    assert!(verdict["reason"].is_null(), "{verdict}");
}

/// Editing a user message is one write (review R1 finding 9): its text, its
/// picks and the cut land together, and a message that is not a user
/// message of the thread writes nothing.
#[tokio::test]
async fn a_user_edit_is_one_write() {
    let state = AppState::init_for_tests().await.unwrap();
    let tid = store::create_chat_thread(&state.db, "m", "chat")
        .await
        .unwrap();
    let u1 = seed(&state, tid, "user", "one").await;
    let r1 = seed(&state, tid, "assistant", "r1").await;
    seed(&state, tid, "user", "two").await;
    let ctx = store::ChatContext {
        query: "one".into(),
        ..Default::default()
    };
    store::set_chat_message_knowledge(&state.db, tid, u1, &[3], Some(&ctx))
        .await
        .unwrap();

    assert!(
        !store::rewrite_chat_user_message(&state.db, tid, r1, "x", &[])
            .await
            .unwrap(),
        "a reply is not rewritten as a user message"
    );
    assert_eq!(
        store::list_chat_messages(&state.db, tid)
            .await
            .unwrap()
            .len(),
        3
    );

    assert!(
        store::rewrite_chat_user_message(&state.db, tid, u1, "one, again", &[4])
            .await
            .unwrap()
    );
    let msgs = store::list_chat_messages(&state.db, tid).await.unwrap();
    assert_eq!(msgs.len(), 1, "everything after it went");
    assert_eq!(msgs[0].content, "one, again");
    assert_eq!(msgs[0].kb_refs, vec![4]);
    assert!(
        msgs[0].context.is_none(),
        "the old retrieval went with the old text"
    );
}

/// A reply records the alias its turn asked for; nothing answered in its
/// place, so `answered_by` is empty (review R1 item c).
#[tokio::test]
async fn a_reply_records_its_model() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "hello", 3, 1).await;
    let (_state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = thread(&gw).await;
    post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi"}),
    )
    .await
    .text()
    .await
    .unwrap();
    let msgs = messages(&gw, tid).await;
    assert_eq!(msgs[1]["model"], "m");
    assert!(msgs[1]["answered_by"].is_null());
    assert!(msgs[0]["model"].is_null(), "a user message has none");
}
