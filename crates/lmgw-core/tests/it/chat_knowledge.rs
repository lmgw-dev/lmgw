//! Knowledge bases in Chat threads (chat-complete design §9.3): auto mode's
//! retrieval before the model is called, the `<context>` block it becomes
//! and its unchanged replay, `#` picks for one message, never blocking a
//! turn, tool mode's restricted `kb__*` tools, and the knowledge fields
//! through settings, folder defaults, temporary chats and Keep.
//!
//! Two mocks: the knowledge fixtures' embedding upstream (`crate::knowledge`)
//! and a chat model `m` of its own, whose requests are read back.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::chat_actions::{get_json, mount_openai_reply, post, sse_events};
use crate::common::{serve, Gw};
use crate::knowledge as kbfix;

const CITE: &str = "Use these excerpts where they are relevant and cite them as [n].";

struct World {
    state: SharedState,
    gw: Gw,
    /// The embedding model's upstream.
    kb_mock: MockServer,
    /// The chat model's upstream.
    chat: MockServer,
}

/// A gateway with the knowledge fixtures' embedding alias and a chat alias
/// `m` on its own mock (no reply mounted yet).
async fn world() -> World {
    let kb_mock = MockServer::start().await;
    kbfix::mount(&kb_mock).await;
    let state = kbfix::setup(&kb_mock).await;
    let chat = MockServer::start().await;
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "chat-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: chat.uri(),
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
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "m".into(),
            upstream_id: up,
            upstream_model_id: "chat-tgt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    World {
        state,
        gw,
        kb_mock,
        chat,
    }
}

impl World {
    async fn base(&self, name: &str, file: (&str, &str)) -> i64 {
        kbfix::ingested(
            &self.state,
            &self.gw,
            json!({"name": name, "embed_alias": "embed-model"}),
            &[(file.0, file.1.as_bytes().to_vec())],
        )
        .await
    }

    async fn thread(&self, extra: Value) -> i64 {
        let mut body = json!({"model_alias": "m"});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().cloned().unwrap_or_default());
        let r = post(&self.gw, "/chat/api/threads", body).await;
        assert_eq!(r.status(), 200);
        r.json::<Value>().await.unwrap()["id"].as_i64().unwrap()
    }

    async fn settings(&self, tid: i64, body: Value) -> (u16, Value) {
        let r = post(&self.gw, &format!("/chat/api/threads/{tid}/settings"), body).await;
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    /// Send and read the whole SSE answer.
    async fn send(&self, tid: i64, body: Value) -> Vec<(String, Value)> {
        let r = post(&self.gw, &format!("/chat/api/threads/{tid}/send"), body).await;
        assert_eq!(r.status(), 200);
        sse_events(&r.text().await.unwrap())
    }

    async fn thread_json(&self, tid: i64) -> Value {
        get_json(&self.gw, &format!("/chat/api/threads/{tid}")).await
    }

    /// Every request the chat model received, as JSON.
    async fn chat_requests(&self) -> Vec<Value> {
        self.chat
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().ends_with("/chat/completions"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    async fn embed_calls(&self) -> usize {
        self.kb_mock
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().ends_with("/embeddings"))
            .count()
    }
}

/// The text of every user message a request carried, in order.
fn user_texts(req: &Value) -> Vec<String> {
    req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| {
            m["content"]
                .as_str()
                .expect("text-only content")
                .to_string()
        })
        .collect()
}

fn event<'a>(events: &'a [(String, Value)], name: &str) -> Option<&'a Value> {
    events.iter().find(|(e, _)| e == name).map(|(_, d)| d)
}

fn position(events: &[(String, Value)], name: &str) -> Option<usize> {
    events.iter().position(|(e, _)| e == name)
}

// ---------------------------------------------------------------------------
// Auto mode
// ---------------------------------------------------------------------------

/// The core promise: the excerpts go out ahead of the question as a numbered,
/// cited block; the event says so before any text; the user message keeps
/// them; the next turn replays that message byte for byte.
#[tokio::test]
async fn auto_mode_sends_the_excerpts_ahead_of_the_question_and_replays_them() {
    let w = world().await;
    mount_openai_reply(&w.chat, "It arrived in May [1].", 10, 5).await;
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let tid = w.thread(json!({})).await;
    let (status, v) = w.settings(tid, json!({"kb_ids": [taxes]})).await;
    assert_eq!(status, 200, "{v}");

    let events = w
        .send(tid, json!({"content": "When did the refund arrive?"}))
        .await;
    let r = event(&events, "retrieval").expect("a retrieval event");
    let at = position(&events, "retrieval").unwrap();
    let first_delta = position(&events, "delta").expect("an answer");
    assert!(at < first_delta, "retrieval comes before the first delta");
    let uid = event(&events, "turn").unwrap()["user_message_id"].clone();
    assert_eq!(r["message_id"], uid);
    assert_eq!(r["reused"], false);
    assert_eq!(r["searched"], json!(["Taxes"]));
    assert_eq!(r["kb_ids"], json!([taxes]));
    assert_eq!(r["budget_tokens"], 4000, "the setting's default budget");
    let excerpts = r["excerpts"].as_array().unwrap();
    assert!(!excerpts.is_empty(), "{r}");
    assert_eq!(excerpts[0]["kb"], "Taxes");
    assert_eq!(excerpts[0]["file"], "notes.md");
    assert!(r["tokens"].as_u64().unwrap() > 0);
    assert!(
        excerpts
            .iter()
            .any(|e| e["text"].as_str().unwrap().contains("412 EUR")),
        "{r}"
    );

    // What the model was sent: the block, then the question.
    let reqs = w.chat_requests().await;
    let first = user_texts(&reqs[0]);
    assert_eq!(first.len(), 1);
    let sent = &first[0];
    assert!(
        sent.starts_with(
            "<context source=\"knowledge\">\n<excerpt n=\"1\" kb=\"Taxes\" file=\"notes.md\""
        ),
        "{sent}"
    );
    assert!(sent.contains("412 EUR"), "{sent}");
    assert!(
        sent.contains(&format!("{CITE}\n</context>\nWhen did the refund arrive?")),
        "{sent}"
    );
    assert!(sent.ends_with("When did the refund arrive?"));

    // Stored with the user message; the reply has none.
    let t = w.thread_json(tid).await;
    let msgs = t["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["kb_refs"], json!([]));
    let stored = &msgs[0]["context"];
    assert_eq!(
        stored["excerpts"], r["excerpts"],
        "the event is what was stored"
    );
    assert_eq!(stored["query"], "When did the refund arrive?");
    assert!(msgs[1]["context"].is_null());
    assert_eq!(t["thread"]["kb_ids"], json!([taxes]));
    assert_eq!(t["thread"]["kb_mode"], "auto");
    assert!(t["thread"]["kb_budget_tokens"].is_null());

    // The next turn replays the first message unchanged, and searches for
    // the follow-up together with the question before it.
    let events = w.send(tid, json!({"content": "And the costs?"})).await;
    let r2 = event(&events, "retrieval").unwrap();
    // Previous message first: retrieval keeps the END of a long query.
    assert_eq!(r2["query"], "When did the refund arrive?\n\nAnd the costs?");
    let reqs = w.chat_requests().await;
    let second = user_texts(&reqs[1]);
    assert_eq!(second.len(), 2);
    assert_eq!(&second[0], sent, "replayed byte for byte");
    assert!(second[1].starts_with("<context source=\"knowledge\">"));
    assert!(second[1].ends_with("And the costs?"));
    kbfix::cleanup(&w.state);
}

/// `#` picks reach one message only: they widen that turn's search and are
/// stored on the message; the next turn is back to the thread's own bases.
#[tokio::test]
async fn kb_refs_add_a_base_for_that_message_only() {
    let w = world().await;
    mount_openai_reply(&w.chat, "ok", 1, 1).await;
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let letters = w.base("Letters", ("letter.txt", kbfix::LETTER)).await;
    let tid = w.thread(json!({})).await;
    w.settings(tid, json!({"kb_ids": [taxes]})).await;

    // An unknown base is refused by id before anything is written.
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "x", "kb_refs": [424242]}),
    )
    .await;
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert!(v["message"].as_str().unwrap().contains("424242"), "{v}");
    assert!(w.thread_json(tid).await["messages"]
        .as_array()
        .unwrap()
        .is_empty());

    let events = w
        .send(
            tid,
            json!({"content": "How much is the rent now?", "kb_refs": [letters, letters]}),
        )
        .await;
    let r = event(&events, "retrieval").unwrap();
    assert_eq!(r["kb_ids"], json!([taxes, letters]));
    let searched: Vec<&str> = r["searched"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(
        searched.contains(&"Taxes") && searched.contains(&"Letters"),
        "{r}"
    );
    assert!(
        r["excerpts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["file"] == "letter.txt"),
        "{r}"
    );
    let t = w.thread_json(tid).await;
    assert_eq!(
        t["messages"][0]["kb_refs"],
        json!([letters]),
        "deduplicated"
    );

    let events = w.send(tid, json!({"content": "And the refund?"})).await;
    let r = event(&events, "retrieval").unwrap();
    assert_eq!(r["kb_ids"], json!([taxes]));
    assert_eq!(r["searched"], json!(["Taxes"]));
    assert!(r["excerpts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["kb"] == "Taxes"));
    kbfix::cleanup(&w.state);
}

/// A thread with no bases (and no `#` pick) searches nothing and sends no
/// block — no event either.
#[tokio::test]
async fn no_bases_means_no_retrieval_and_no_block() {
    let w = world().await;
    mount_openai_reply(&w.chat, "plain", 1, 1).await;
    let tid = w.thread(json!({})).await;
    let events = w.send(tid, json!({"content": "hello"})).await;
    assert!(event(&events, "retrieval").is_none(), "{events:?}");
    let reqs = w.chat_requests().await;
    assert_eq!(user_texts(&reqs[0]), vec!["hello".to_string()]);
    let t = w.thread_json(tid).await;
    assert!(t["messages"][0]["context"].is_null());
    assert!(w.embed_calls().await == 0);
    kbfix::cleanup(&w.state);
}

/// Retrieval never blocks a turn: a base whose embedding model is gone is
/// searched by keyword and says so; a base deleted since it was picked finds
/// nothing and says so — and the model answers either way.
#[tokio::test]
async fn a_failing_retrieval_explains_itself_and_the_turn_still_answers() {
    let w = world().await;
    mount_openai_reply(&w.chat, "answered anyway", 1, 1).await;
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let tid = w.thread(json!({})).await;
    w.settings(tid, json!({"kb_ids": [taxes]})).await;

    // The alias that pinned the base is gone.
    let alias = w
        .state
        .snapshot()
        .aliases
        .values()
        .find(|a| a.alias == "embed-model")
        .unwrap()
        .id;
    store::delete_alias(&w.state.db, alias).await.unwrap();
    w.state.reload_snapshot().await.unwrap();

    let events = w.send(tid, json!({"content": "refund May"})).await;
    let r = event(&events, "retrieval").unwrap();
    assert!(
        r["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("keyword search only")),
        "{r}"
    );
    assert!(
        !r["excerpts"].as_array().unwrap().is_empty(),
        "BM25 still answers: {r}"
    );
    let done = event(&events, "done").unwrap();
    assert_eq!(done["aborted"], false, "{events:?}");
    assert!(events
        .iter()
        .any(|(e, d)| e == "delta" && d["text"].as_str().is_some()));

    // The base itself is gone now.
    lmgw_core::knowledge::ops::delete(&w.state, taxes)
        .await
        .unwrap();
    let events = w.send(tid, json!({"content": "and now?"})).await;
    let r = event(&events, "retrieval").unwrap();
    assert!(r["excerpts"].as_array().unwrap().is_empty());
    assert!(
        r["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("no longer exists")),
        "{r}"
    );
    assert_eq!(event(&events, "done").unwrap()["aborted"], false);
    let reqs = w.chat_requests().await;
    let last = user_texts(reqs.last().unwrap());
    assert_eq!(last.last().unwrap(), "and now?", "nothing found, no block");
    // Saving other settings is not blocked by the base that went away.
    let (status, v) = w
        .settings(tid, json!({"kb_ids": [taxes], "temperature": 0.2}))
        .await;
    assert_eq!(status, 200, "{v}");
    kbfix::cleanup(&w.state);
}

/// Regenerate answers the same question again with the same excerpts (no new
/// search); editing the question searches again.
#[tokio::test]
async fn regenerate_reuses_the_retrieval_and_an_edit_searches_again() {
    let w = world().await;
    mount_openai_reply(&w.chat, "reply", 1, 1).await;
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let letters = w.base("Letters", ("letter.txt", kbfix::LETTER)).await;
    let tid = w.thread(json!({})).await;
    w.settings(tid, json!({"kb_ids": [taxes]})).await;
    w.send(tid, json!({"content": "When did the refund arrive?"}))
        .await;
    let t = w.thread_json(tid).await;
    let uid = t["messages"][0]["id"].as_i64().unwrap();
    let rid = t["messages"][1]["id"].as_i64().unwrap();
    let stored = t["messages"][0]["context"].clone();
    let embeds = w.embed_calls().await;

    for target in [rid, uid] {
        let r = post(
            &w.gw,
            &format!("/chat/api/threads/{tid}/messages/{target}/regenerate"),
            json!({}),
        )
        .await;
        assert_eq!(r.status(), 200);
        let events = sse_events(&r.text().await.unwrap());
        let ev = event(&events, "retrieval").expect("the stored retrieval is shown again");
        assert_eq!(ev["reused"], true);
        assert_eq!(ev["message_id"], uid);
        assert_eq!(ev["excerpts"], stored["excerpts"]);
    }
    assert_eq!(w.embed_calls().await, embeds, "nothing was searched again");
    let reqs = w.chat_requests().await;
    assert_eq!(user_texts(&reqs[1]), user_texts(&reqs[0]));
    assert_eq!(user_texts(&reqs[2]), user_texts(&reqs[0]));

    // An edit searches for the new text — with the `#` picks it now names.
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{uid}/edit"),
        json!({"content": "What does the flat cost?", "kb_refs": [letters]}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let events = sse_events(&r.text().await.unwrap());
    let ev = event(&events, "retrieval").unwrap();
    assert_eq!(ev["reused"], false);
    assert_eq!(ev["query"], "What does the flat cost?");
    assert_eq!(ev["kb_ids"], json!([taxes, letters]));
    assert!(w.embed_calls().await > embeds);
    let t = w.thread_json(tid).await;
    assert_eq!(t["messages"][0]["kb_refs"], json!([letters]));
    assert_eq!(
        t["messages"][0]["context"]["query"],
        "What does the flat cost?"
    );

    // An edit that sends no `kb_refs` keeps them; an unknown one is refused.
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{uid}/edit"),
        json!({"content": "x", "kb_refs": [999]}),
    )
    .await;
    assert_eq!(r.status(), 400);
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{uid}/edit"),
        json!({"content": "The rent?"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let _ = r.text().await;
    let t = w.thread_json(tid).await;
    assert_eq!(t["messages"][0]["kb_refs"], json!([letters]));
    kbfix::cleanup(&w.state);
}

// ---------------------------------------------------------------------------
// Tool mode
// ---------------------------------------------------------------------------

fn tool_call_sse(id: &str, name: &str, args: Value) -> String {
    let start = json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": id,
        "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}}]});
    let stop = json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]});
    format!("data: {start}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
}

/// Tool mode attaches `kb__list` / `kb__search` / `kb__read` even with no
/// MCP server, searches nothing up front, and the tools reach exactly the
/// thread's bases: another base answers like one that does not exist.
#[tokio::test]
async fn tool_mode_gives_the_model_kb_tools_restricted_to_the_thread() {
    let w = world().await;
    let answer = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let replies = [
        tool_call_sse("c1", "kb__search", json!({"query": "rent landlord"})),
        tool_call_sse(
            "c2",
            "kb__search",
            json!({"query": "rent", "kb": "Letters"}),
        ),
        answer.to_string(),
    ];
    for (i, body) in replies.into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .mount(&w.chat)
            .await;
    }
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let _letters = w.base("Letters", ("letter.txt", kbfix::LETTER)).await;
    let tid = w.thread(json!({})).await;
    let (status, v) = w
        .settings(
            tid,
            json!({"kb_ids": [taxes], "kb_mode": "tool", "kb_budget_tokens": 300}),
        )
        .await;
    assert_eq!(status, 200, "{v}");

    let events = w.send(tid, json!({"content": "what is the rent?"})).await;
    assert!(event(&events, "retrieval").is_none(), "no pre-retrieval");
    let results: Vec<&Value> = events
        .iter()
        .filter(|(e, d)| e == "tool" && d["event"] == "result")
        .map(|(_, d)| d)
        .collect();
    assert_eq!(results.len(), 2, "{events:?}");
    let first = results[0]["output"].as_str().unwrap();
    assert_eq!(results[0]["name"], "kb__search");
    assert_eq!(results[0]["is_error"], false, "{first}");
    assert!(first.contains("Searched: Taxes"), "{first}");
    assert!(
        first.contains("(budget 300)"),
        "the thread's budget: {first}"
    );
    assert!(!first.contains("letter.txt"), "{first}");
    let second = results[1]["output"].as_str().unwrap();
    assert_eq!(results[1]["is_error"], true, "{second}");
    assert!(second.contains("no knowledge base 'Letters'"), "{second}");
    assert!(events
        .iter()
        .any(|(e, d)| e == "delta" && d["text"] == "done"));

    let reqs = w.chat_requests().await;
    let mut names: Vec<&str> = reqs[0]["tools"]
        .as_array()
        .expect("tools are sent")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    names.sort();
    assert_eq!(names, ["kb__list", "kb__read", "kb__search"]);
    assert_eq!(user_texts(&reqs[0]), vec!["what is the rent?".to_string()]);
    kbfix::cleanup(&w.state);
}

/// A device's thread with the `kb` label attached by hand (tool mode off):
/// its `kb__search` embeds as the device, checked against its key and
/// charged to it, never as the gateway (client-apps design L4, review W3-3).
#[tokio::test]
async fn a_device_s_hand_attached_kb_label_searches_as_the_device() {
    let w = world().await;
    let answer = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let replies = [
        tool_call_sse("c1", "kb__search", json!({"query": "refund"})),
        answer.to_string(),
    ];
    for (i, body) in replies.into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .mount(&w.chat)
            .await;
    }
    w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let r = post(
        &w.gw,
        "/api/op/key_create",
        json!({ "kind": "device", "name": "phone" }),
    )
    .await;
    assert_eq!(r.status(), 200);
    let paired: Value = r.json().await.unwrap();
    let device_id = paired["id"].as_i64().unwrap();
    let device = crate::device_chat::bearer(paired["key"].as_str().unwrap());
    let created: Value = device
        .post(format!("{}/chat/api/threads", w.gw))
        .json(&json!({ "model_alias": "m" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = created["id"].as_i64().unwrap();
    let r = device
        .post(format!("{}/chat/api/threads/{tid}/settings", w.gw))
        .json(&json!({ "mcp_tools": [{ "server_label": "kb" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let r = device
        .post(format!("{}/chat/api/threads/{tid}/send", w.gw))
        .json(&json!({ "content": "the refund?" }))
        .send()
        .await
        .unwrap();
    let events = sse_events(&r.text().await.unwrap());
    let result = events
        .iter()
        .find(|(e, d)| e == "tool" && d["event"] == "result")
        .map(|(_, d)| d.clone())
        .unwrap_or_else(|| panic!("no tool result: {events:?}"));
    assert_eq!(result["is_error"], false, "{result}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let embeds: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_logs WHERE key_id = ?1 AND requested_alias = 'embed-model'",
    )
    .bind(device_id)
    .fetch_one(&w.state.db)
    .await
    .unwrap();
    assert!(embeds >= 1, "the search's embedding is the device's");
    // The tool loop's two model calls are two rows of the device's (review
    // W3-11: each call checked and counted).
    let calls: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_logs WHERE key_id = ?1 AND requested_alias = 'm'",
    )
    .bind(device_id)
    .fetch_one(&w.state.db)
    .await
    .unwrap();
    assert_eq!(calls, 2);
    kbfix::cleanup(&w.state);
}

/// A device's auto-mode retrieval embeds as the device (review W3-11): the
/// query's embedding row is the device key's, not the gateway's.
#[tokio::test]
async fn a_device_s_auto_retrieval_embeds_as_the_device() {
    let w = world().await;
    mount_openai_reply(&w.chat, "done", 1, 1).await;
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let r = post(
        &w.gw,
        "/api/op/key_create",
        json!({ "kind": "device", "name": "phone" }),
    )
    .await;
    assert_eq!(r.status(), 200);
    let paired: Value = r.json().await.unwrap();
    let device_id = paired["id"].as_i64().unwrap();
    let device = crate::device_chat::bearer(paired["key"].as_str().unwrap());
    let created: Value = device
        .post(format!("{}/chat/api/threads", w.gw))
        .json(&json!({ "model_alias": "m" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = created["id"].as_i64().unwrap();
    let r = device
        .post(format!("{}/chat/api/threads/{tid}/settings", w.gw))
        .json(&json!({ "kb_ids": [taxes], "kb_mode": "auto" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let r = device
        .post(format!("{}/chat/api/threads/{tid}/send", w.gw))
        .json(&json!({ "content": "when did the refund arrive?" }))
        .send()
        .await
        .unwrap();
    let events = sse_events(&r.text().await.unwrap());
    assert!(event(&events, "retrieval").is_some(), "{events:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT requested_alias, status FROM request_logs WHERE key_id = ?1 ORDER BY id",
    )
    .bind(device_id)
    .fetch_all(&w.state.db)
    .await
    .unwrap();
    assert!(
        rows.contains(&("embed-model".to_string(), 200)),
        "the retrieval's embedding is the device's: {rows:?}"
    );
    assert!(rows.contains(&("m".to_string(), 200)), "{rows:?}");
    kbfix::cleanup(&w.state);
}

// ---------------------------------------------------------------------------
// Settings, folders, temporary chats, Keep
// ---------------------------------------------------------------------------

#[tokio::test]
async fn thread_settings_check_the_knowledge_fields() {
    let w = world().await;
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let tid = w.thread(json!({})).await;

    let (status, v) = w
        .settings(tid, json!({"kb_ids": [taxes, 31337, 4242]}))
        .await;
    assert_eq!(status, 400);
    let msg = v["message"].as_str().unwrap();
    assert!(msg.contains("31337, 4242"), "{msg}");
    let (status, v) = w.settings(tid, json!({"kb_mode": "sometimes"})).await;
    assert_eq!(status, 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains("kb_mode"));
    let (status, v) = w.settings(tid, json!({"kb_budget_tokens": 0})).await;
    assert_eq!(status, 400, "{v}");
    let t = w.thread_json(tid).await["thread"].clone();
    assert_eq!(t["kb_ids"], json!([]), "nothing was applied");
    assert_eq!(t["kb_mode"], "auto");

    let (status, v) = w
        .settings(
            tid,
            json!({"kb_ids": [taxes, taxes], "kb_mode": "tool", "kb_budget_tokens": 1200}),
        )
        .await;
    assert_eq!(status, 200, "{v}");
    let t = w.thread_json(tid).await["thread"].clone();
    assert_eq!(t["kb_ids"], json!([taxes]));
    assert_eq!(t["kb_mode"], "tool");
    assert_eq!(t["kb_budget_tokens"], 1200);

    // Absent keeps, null returns the budget to the setting, [] clears.
    let (status, _) = w.settings(tid, json!({"temperature": 0.5})).await;
    assert_eq!(status, 200);
    assert_eq!(w.thread_json(tid).await["thread"]["kb_budget_tokens"], 1200);
    w.settings(tid, json!({"kb_budget_tokens": null, "kb_ids": []}))
        .await;
    let t = w.thread_json(tid).await["thread"].clone();
    assert!(t["kb_budget_tokens"].is_null());
    assert_eq!(t["kb_ids"], json!([]));
    assert_eq!(t["kb_mode"], "tool");
    kbfix::cleanup(&w.state);
}

#[tokio::test]
async fn folder_defaults_carry_the_knowledge_settings_into_a_new_thread() {
    let w = world().await;
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;

    let r = post(
        &w.gw,
        "/chat/api/folders",
        json!({"name": "Bad", "defaults": {"kb_ids": [777]}}),
    )
    .await;
    assert_eq!(r.status(), 400);
    let r = post(
        &w.gw,
        "/chat/api/folders",
        json!({"name": "Bad", "defaults": {"kb_mode": "never"}}),
    )
    .await;
    assert_eq!(r.status(), 400);

    let r = post(
        &w.gw,
        "/chat/api/folders",
        json!({"name": "Plain", "defaults": {"kb_ids": [], "kb_mode": "auto"}}),
    )
    .await;
    let plain: Value = r.json().await.unwrap();
    assert!(plain["defaults"]["kb_ids"].is_null(), "{plain}");
    assert!(plain["defaults"]["kb_mode"].is_null(), "{plain}");

    let r = post(
        &w.gw,
        "/chat/api/folders",
        json!({"name": "Taxes", "defaults":
            {"kb_ids": [taxes], "kb_mode": "tool", "kb_budget_tokens": 1500}}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let folder: Value = r.json().await.unwrap();
    let fid = folder["id"].as_i64().unwrap();
    assert_eq!(folder["defaults"]["kb_ids"], json!([taxes]));

    let r = post(
        &w.gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "folder_id": fid}),
    )
    .await;
    let t: Value = r.json().await.unwrap();
    assert_eq!(t["kb_ids"], json!([taxes]));
    assert_eq!(t["kb_mode"], "tool");
    assert_eq!(t["kb_budget_tokens"], 1500);

    // A base the folder already names is not checked again on a later save.
    lmgw_core::knowledge::ops::delete(&w.state, taxes)
        .await
        .unwrap();
    let r = post(
        &w.gw,
        &format!("/chat/api/folders/{fid}"),
        json!({"name": "Taxes (old)", "defaults": {"kb_ids": [taxes], "kb_mode": "tool"}}),
    )
    .await;
    assert_eq!(r.status(), 200);
    kbfix::cleanup(&w.state);
}

/// A temporary chat retrieves like any other, and Keep writes the knowledge
/// settings and each message's `kb_refs` and `context` with it — the kept
/// thread replays the same block.
#[tokio::test]
async fn a_temporary_chat_retrieves_and_keep_copies_the_context() {
    let w = world().await;
    mount_openai_reply(&w.chat, "ok", 1, 1).await;
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let letters = w.base("Letters", ("letter.txt", kbfix::LETTER)).await;
    let tid = w.thread(json!({"temporary": true})).await;
    assert!(tid < 0);
    let (status, v) = w
        .settings(tid, json!({"kb_ids": [taxes], "kb_budget_tokens": 2000}))
        .await;
    assert_eq!(status, 200, "{v}");
    let events = w
        .send(
            tid,
            json!({"content": "rent and refund", "kb_refs": [letters]}),
        )
        .await;
    let r = event(&events, "retrieval").unwrap();
    assert_eq!(r["budget_tokens"], 2000);
    assert!(!r["excerpts"].as_array().unwrap().is_empty());
    let temp = w.thread_json(tid).await;
    let context = temp["messages"][0]["context"].clone();
    assert_eq!(context["excerpts"], r["excerpts"]);

    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/persist"),
        json!({}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let kept: Value = r.json().await.unwrap();
    let kid = kept["id"].as_i64().unwrap();
    let t = w.thread_json(kid).await;
    assert_eq!(t["thread"]["kb_ids"], json!([taxes]));
    assert_eq!(t["thread"]["kb_budget_tokens"], 2000);
    assert_eq!(t["thread"]["kb_mode"], "auto");
    assert_eq!(t["messages"][0]["kb_refs"], json!([letters]));
    assert_eq!(t["messages"][0]["context"], context);

    w.send(kid, json!({"content": "thanks"})).await;
    let reqs = w.chat_requests().await;
    assert_eq!(
        user_texts(&reqs[1])[0],
        user_texts(&reqs[0])[0],
        "the kept thread replays the temporary turn's block"
    );
    kbfix::cleanup(&w.state);
}

// ---------------------------------------------------------------------------
// A turn still retrieving when the history is rewritten (review N1)
// ---------------------------------------------------------------------------

/// The embedding answer of the fixtures, after `delay`.
struct SlowEmbeddings(Duration);

impl Respond for SlowEmbeddings {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        kbfix::FixtureEmbeddings.respond(req).set_delay(self.0)
    }
}

/// A turn whose retrieval is slow (a second tab) is overtaken by an edit of
/// its message: the rewrite cancels it at once, it says so, and it writes
/// nothing — the edited message holds the *new* text's context, and the next
/// turn reuses that one instead of replaying the old text's forever.
#[tokio::test]
async fn a_slow_retrieval_never_writes_the_old_context_onto_an_edit() {
    let w = world().await;
    mount_openai_reply(&w.chat, "reply", 1, 1).await;
    let taxes = w.base("Taxes", ("notes.md", kbfix::NOTES)).await;
    let tid = w.thread(json!({})).await;
    w.settings(tid, json!({"kb_ids": [taxes]})).await;

    // The first embedding request (T1's) takes long; later ones are fast.
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(SlowEmbeddings(Duration::from_millis(1500)))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&w.kb_mock)
        .await;
    let r1 = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "old text about the refund"}),
    )
    .await;
    assert_eq!(r1.status(), 200);
    let t1 = tokio::spawn(async move { sse_events(&r1.text().await.unwrap()) });
    // T1 is in its retrieval; its user message is stored.
    let mut uid = 0;
    for _ in 0..50 {
        let t = w.thread_json(tid).await;
        if let Some(m) = t["messages"].as_array().and_then(|m| m.first()) {
            uid = m["id"].as_i64().unwrap();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(uid, 0);

    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{uid}/edit"),
        json!({"content": "What does the flat cost?"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let edited = sse_events(&r.text().await.unwrap());
    assert_eq!(event(&edited, "retrieval").unwrap()["reused"], false);

    let first = t1.await.unwrap();
    assert!(
        event(&first, "error").is_some() && event(&first, "retrieval").is_none(),
        "the overtaken turn stops at the rewrite: {first:?}"
    );
    // Let the slow embedding finish; had T1 kept going it would write now.
    tokio::time::sleep(Duration::from_millis(1700)).await;
    let t = w.thread_json(tid).await;
    assert_eq!(
        t["messages"][0]["context"]["query"], "What does the flat cost?",
        "the edited message holds the new text's context"
    );
    assert_eq!(
        t["messages"].as_array().unwrap().len(),
        2,
        "[user', reply']"
    );

    // The next turn reuses that context (a regenerate), and it is the fresh one.
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{uid}/regenerate"),
        json!({}),
    )
    .await;
    let events = sse_events(&r.text().await.unwrap());
    let ev = event(&events, "retrieval").unwrap();
    assert_eq!(ev["reused"], true);
    assert_eq!(ev["query"], "What does the flat cost?");
    kbfix::cleanup(&w.state);
}
