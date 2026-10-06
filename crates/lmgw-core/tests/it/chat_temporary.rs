//! Temporary chats (chat-complete design §7): created with `{temporary:
//! true}`, listed apart, never written to the DB — sends, attachments and
//! settings included — until **Keep** (`…/persist`) writes the whole thread as
//! an ordinary one; delete discards it.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, ThreadListMode};
use serde_json::{json, Value};
use wiremock::MockServer;

use crate::chat_actions::{gateway, get_json, mount_openai_reply, post, sse_events};
use crate::common::Gw;

async fn temporary(gw: &Gw) -> Value {
    post(
        gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "temporary": true}),
    )
    .await
    .json()
    .await
    .unwrap()
}

/// What the DB holds of the Chat, whole: no temporary thread may reach it.
async fn stored_rows(state: &SharedState) -> (usize, usize) {
    let threads = store::list_chat_threads(&state.db, ThreadListMode::All)
        .await
        .unwrap();
    let mut messages = 0;
    for t in &threads {
        messages += store::list_chat_messages(&state.db, t.id)
            .await
            .unwrap()
            .len();
    }
    (threads.len(), messages)
}

#[tokio::test]
async fn a_temporary_thread_is_negative_listed_apart_and_never_stored() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "hello there", 5, 2).await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;

    // `kind` is ignored: a temporary chat is always a plain one.
    let t: Value = post(
        &gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "kind": "admin", "temporary": true}),
    )
    .await
    .json()
    .await
    .unwrap();
    let tid = t["id"].as_i64().unwrap();
    assert!(tid < 0, "{t}");
    assert_eq!(t["temporary"], true);
    assert_eq!(t["kind"], "chat");
    assert_eq!(t["title"], "New chat");

    let list = get_json(&gw, "/chat/api/threads").await;
    assert_eq!(list["threads"].as_array().unwrap().len(), 0, "{list}");
    assert_eq!(list["temporary"][0]["id"], tid, "{list}");
    // The archived list carries them too: the array is its own.
    let archived = get_json(&gw, "/chat/api/threads?archived=1").await;
    assert_eq!(archived["temporary"][0]["id"], tid, "{archived}");

    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi"}),
    )
    .await
    .text()
    .await
    .unwrap();
    let events = sse_events(&body);
    assert_eq!(events[0].0, "turn", "{body}");
    let user_id = events[0].1["user_message_id"].as_i64().unwrap();
    assert!(user_id < 0, "{body}");
    let done = &events.last().unwrap().1;
    let reply_id = done["message_id"].as_i64().unwrap();
    assert!(reply_id < 0 && reply_id != user_id, "{body}");

    let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    let msgs = detail["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "{detail}");
    assert_eq!(msgs[0]["id"], user_id);
    assert_eq!(msgs[1]["id"], reply_id);
    assert_eq!(msgs[1]["content"], "hello there");
    assert_eq!(msgs[1]["completion_tokens"], 2);
    assert_eq!(
        detail["thread"]["title"], "hi",
        "auto-titled like any thread"
    );
    assert_eq!(detail["thread"]["temporary"], true);

    // Settings patch the in-memory thread.
    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"temperature": 0.3}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(detail["thread"]["temperature"], 0.3, "{detail}");

    // Nothing of it is in the DB — but the request log has its row, as for
    // any turn (it holds no content).
    assert_eq!(stored_rows(&state).await, (0, 0));
    let logs = store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    assert!(logs.iter().any(|l| l.ingress_proto == "chat"), "{logs:?}");
}

#[tokio::test]
async fn keep_writes_the_whole_thread_and_discards_it_from_memory() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "a reply", 9, 3).await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = temporary(&gw).await["id"].as_i64().unwrap();

    let att: Value = gw
        .client()
        .post(format!(
            "{gw}/chat/api/threads/{tid}/attachments?name=notes.txt"
        ))
        .body("some notes")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let aid = att["id"].as_i64().unwrap();
    assert!(aid < 0, "{att}");
    let bytes = gw
        .client()
        .get(format!("{gw}/chat/api/attachments/{aid}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(bytes, "some notes");

    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "read this", "attachments": [aid]}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert!(body.contains("event: done"), "{body}");
    // A second draft, left unsent: Keep takes it along as a draft.
    let draft: Value = gw
        .client()
        .post(format!(
            "{gw}/chat/api/threads/{tid}/attachments?name=later.txt"
        ))
        .body("for later")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(draft["id"].as_i64().unwrap() < 0);
    post(
        &gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"system_prompt": "be brief", "max_tokens": 64}),
    )
    .await;

    let kept: Value = post(&gw, &format!("/chat/api/threads/{tid}/persist"), json!({}))
        .await
        .json()
        .await
        .unwrap();
    let new_id = kept["id"].as_i64().unwrap();
    assert!(new_id > 0, "{kept}");
    assert_eq!(kept["thread"]["id"], new_id);
    assert_eq!(kept["thread"]["temporary"], false);

    // Gone from memory…
    let r = gw
        .client()
        .get(format!("{gw}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let list = get_json(&gw, "/chat/api/threads").await;
    assert!(list["temporary"].as_array().unwrap().is_empty(), "{list}");
    assert_eq!(list["threads"][0]["id"], new_id, "{list}");

    // …and whole in the DB: settings, both messages in order, the sent
    // file bound to the user message, the draft still a draft.
    let detail = get_json(&gw, &format!("/chat/api/threads/{new_id}")).await;
    assert_eq!(detail["thread"]["system_prompt"], "be brief", "{detail}");
    assert_eq!(detail["thread"]["max_tokens"], 64);
    assert_eq!(detail["thread"]["title"], "read this");
    let msgs = detail["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "{detail}");
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["attachments"][0]["name"], "notes.txt", "{detail}");
    assert_eq!(msgs[1]["content"], "a reply");
    assert_eq!(msgs[1]["prompt_tokens"], 9);
    assert_eq!(detail["draft_attachments"][0]["name"], "later.txt");
    let sent_id = msgs[0]["attachments"][0]["id"].as_i64().unwrap();
    assert!(sent_id > 0);
    let bytes = gw
        .client()
        .get(format!("{gw}/chat/api/attachments/{sent_id}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(bytes, "some notes");
    assert_eq!(stored_rows(&state).await, (1, 2));

    // Keeping it twice is the stored thread's refusal, not a second copy.
    let r = post(
        &gw,
        &format!("/chat/api/threads/{new_id}/persist"),
        json!({}),
    )
    .await;
    assert_eq!(r.status(), 409);
    let r = post(&gw, &format!("/chat/api/threads/{tid}/persist"), json!({})).await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn a_temporary_thread_cannot_be_pinned_or_archived_and_delete_discards_it() {
    let mock = MockServer::start().await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    let tid = temporary(&gw).await["id"].as_i64().unwrap();

    for (route, body) in [
        ("pin", json!({"pinned": true})),
        ("archive", json!({"archived": true})),
    ] {
        let r = post(&gw, &format!("/chat/api/threads/{tid}/{route}"), body).await;
        assert_eq!(r.status(), 409, "{route}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["code"], "temporary_thread", "{v}");
        assert!(v["message"].as_str().unwrap().contains("Keep"), "{v}");
    }

    let r = post(&gw, &format!("/chat/api/threads/{tid}/delete"), json!({})).await;
    assert_eq!(r.status(), 200);
    let r = gw
        .client()
        .get(format!("{gw}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let list = get_json(&gw, "/chat/api/threads").await;
    assert!(list["temporary"].as_array().unwrap().is_empty(), "{list}");
    assert_eq!(stored_rows(&state).await, (0, 0));
}

/// Message actions dispatch on the thread's sign like every other route: a
/// temporary thread is rewritten, answered again and continued in memory.
#[tokio::test]
async fn message_actions_work_on_a_temporary_thread() {
    let mock = MockServer::start().await;
    mount_openai_reply(&mock, "ok then", 4, 2).await;
    let (state, gw) = gateway(&mock, UpstreamKind::LlamaServer, Protocol::LlamaCpp).await;
    let tid = temporary(&gw).await["id"].as_i64().unwrap();
    let send = |q: &'static str| {
        let gw = &gw;
        async move {
            let body = post(
                gw,
                &format!("/chat/api/threads/{tid}/send"),
                json!({"content": q}),
            )
            .await
            .text()
            .await
            .unwrap();
            let events = sse_events(&body);
            let user = events[0].1["user_message_id"].as_i64().unwrap();
            let reply = events.last().unwrap().1["message_id"].as_i64().unwrap();
            (user, reply)
        }
    };
    let (q1, a1) = send("q1").await;
    send("q2").await;
    let contents = |v: &Value| -> Vec<String> {
        v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap().to_string())
            .collect()
    };

    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{a1}/regenerate"),
        json!({}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert!(body.contains("event: done"), "{body}");
    let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(contents(&detail), ["q1", "ok then"], "{detail}");

    let body = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{q1}/edit"),
        json!({"content": "q1, better"}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert_eq!(sse_events(&body)[0].1["user_message_id"], q1, "{body}");
    let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(contents(&detail), ["q1, better", "ok then"]);
    assert_eq!(detail["thread"]["continue"]["ok"], true, "{detail}");

    let body = post(&gw, &format!("/chat/api/threads/{tid}/continue"), json!({}))
        .await
        .text()
        .await
        .unwrap();
    assert!(body.contains("event: done"), "{body}");
    let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(contents(&detail), ["q1, better", "ok thenok then"]);

    let reply = detail["messages"][1]["id"].as_i64().unwrap();
    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{reply}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let detail = get_json(&gw, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(contents(&detail), ["q1, better"]);
    assert_eq!(stored_rows(&state).await, (0, 0));
}

/// Two Keeps of one temporary thread at once store it once (review R1
/// finding 8): the thread leaves memory before the insert, so the second
/// finds it busy (or already gone), never a second copy.
#[tokio::test]
async fn keeping_twice_at_once_stores_it_once() {
    let mock = MockServer::start().await;
    let (state, gw) = gateway(&mock, UpstreamKind::Generic, Protocol::Openai).await;
    for _ in 0..5 {
        let tid = temporary(&gw).await["id"].as_i64().unwrap();
        let before = stored_rows(&state).await.0;
        let route = format!("/chat/api/threads/{tid}/persist");
        let (a, b) = tokio::join!(post(&gw, &route, json!({})), post(&gw, &route, json!({})));
        let mut statuses = vec![a.status().as_u16(), b.status().as_u16()];
        statuses.sort();
        assert_eq!(statuses[0], 200, "{statuses:?}");
        assert!(matches!(statuses[1], 404 | 409), "{statuses:?}");
        assert_eq!(stored_rows(&state).await.0, before + 1, "stored once");
        let again = post(&gw, &route, json!({})).await;
        assert_eq!(again.status(), 404, "gone once kept");
    }
}
