//! A device's turn runs as its key (client-apps design L4, §1.3): every
//! model call is checked against the key and charged to it — the chat
//! model, the tool loop's calls, ASR and TTS — its tools resolve under its
//! tool scope, and the routes that call a model outside a turn (dictation,
//! read-aloud, warm, an attachment's transcript; review W2-3) do too.

use serde_json::{json, Value};

use super::{chat_thread, device_world, op, pair, post, rows_of, sse};
use crate::realtime_chat_thread::world;
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::Turn;
use crate::support::realtime_tts::{speech, wav};

fn error_codes(frames: &[(String, Value)]) -> Vec<String> {
    frames
        .iter()
        .filter(|(e, _)| e == "error")
        .map(|(_, d)| d["code"].as_str().unwrap_or("").to_string())
        .collect()
}

#[tokio::test]
async fn a_device_s_turn_is_charged_to_its_key() {
    let (w, d) = device_world().await;
    w.chat.push(Turn::text(&["Hello."]));
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let (s, frames) = sse(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "hi" }),
    )
    .await;
    assert_eq!(s, 200);
    assert!(frames.iter().any(|(e, _)| e == "done"), "{frames:?}");
    assert_eq!(
        rows_of(&w, d.id).await,
        vec![("chat".into(), "chatty".into(), 200, None)],
        "the row is the device's, not internal:chat's"
    );

    // The owner's own turn keeps today's charging.
    w.chat.push(Turn::text(&["Hi."]));
    let mine = chat_thread(&w, &w.gw.client(), "chatty").await;
    sse(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{mine}/send"),
        json!({ "content": "hi" }),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let charged: Option<String> = sqlx::query_scalar(
        "SELECT k.name FROM request_logs r JOIN api_keys k ON k.id = r.key_id \
         WHERE r.ingress_proto = 'chat' ORDER BY r.id DESC LIMIT 1",
    )
    .fetch_optional(&w.state.db)
    .await
    .unwrap();
    assert_eq!(charged.as_deref(), Some("internal:chat"));
}

#[tokio::test]
async fn an_alias_outside_the_scope_is_refused_at_the_call() {
    let w = world(|_| {}).await;
    let d = pair(
        &w,
        "phone",
        json!({ "scope_mode": "allow", "scope_patterns": "other" }),
    )
    .await;
    // The owner's thread: a device cannot write a model outside its scope
    // (`writes`), but may send into a thread the owner set to one.
    let tid = chat_thread(&w, &w.gw.client(), "chatty").await;
    let (s, frames) = sse(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "hi" }),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(error_codes(&frames), vec!["key_scope"], "{frames:?}");
    assert_eq!(w.chat.seen.chat_count(), 0, "the model was never called");
    let rows = rows_of(&w, d.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].2, rows[0].3.as_deref()), (403, Some("key_scope")));
}

#[tokio::test]
async fn a_tool_turn_outside_the_scope_is_refused_before_anything_is_loaded() {
    let w = world(|_| {}).await;
    // Its tool scope keeps the thread's toolset out too: the loop would
    // report that and stop, after its resolve — the key's check comes first.
    let d = pair(
        &w,
        "phone",
        json!({
            "scope_mode": "allow", "scope_patterns": "other", "rpm_limit": 1,
            "tool_scope_mode": "allow", "tool_scope_patterns": "pfx__*"
        }),
    )
    .await;
    // The owner's thread on `chatty`, with a toolset: a send runs the tool
    // loop, whose resolve and GPU admission must not come before the key's
    // check (review W3-2).
    let tid = chat_thread(&w, &w.gw.client(), "chatty").await;
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "docs" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, frames) = sse(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "hi" }),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(error_codes(&frames), vec!["key_scope"], "{frames:?}");
    assert!(
        !frames.iter().any(|(e, _)| e == "state"),
        "nothing was loaded for it: {frames:?}"
    );
    assert_eq!(w.chat.seen.chat_count(), 0, "the model was never called");
    let rows = rows_of(&w, d.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].2, rows[0].3.as_deref()), (403, Some("key_scope")));
    // Not counted: the minute's one call is still the device's.
    let mine = chat_thread(&w, &w.gw.client(), "other").await;
    w.chat.push(Turn::text(&["Fine."]));
    let (_, frames) = sse(
        &w,
        &d.client,
        &format!("/chat/api/threads/{mine}/send"),
        json!({ "content": "hi" }),
    )
    .await;
    assert_eq!(error_codes(&frames), Vec::<String>::new(), "{frames:?}");
}

/// `concurrency_limit` bounds a device's Chat turns (review W3-8): a turn
/// holds one slot for its length; a second one at the limit is the key's
/// own `429 key_rate`, its row written; the slot is free once the turn ends.
#[tokio::test]
async fn a_device_s_turns_hold_its_concurrency_slots() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({ "concurrency_limit": 1 })).await;
    let hold = std::sync::Arc::new(tokio::sync::Notify::new());
    w.chat.push(Turn::Stream(vec![
        crate::support::realtime_fakes::Step::Text("Thinking"),
        crate::support::realtime_fakes::Step::Wait(hold.clone()),
        crate::support::realtime_fakes::Step::Finish("stop"),
        crate::support::realtime_fakes::Step::Usage(3, 2),
    ]));
    let first = chat_thread(&w, &d.client, "chatty").await;
    let second = chat_thread(&w, &d.client, "chatty").await;
    let running = d
        .client
        .post(format!("{}/chat/api/threads/{first}/send", w.gw))
        .json(&json!({ "content": "a long one" }))
        .send()
        .await
        .unwrap();
    assert_eq!(running.status(), 200);
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/threads/{second}/send"),
        json!({ "content": "and another" }),
    )
    .await;
    assert_eq!((s, &v["code"]), (429, &json!("key_rate")), "{v}");
    assert!(v["message"].as_str().unwrap().contains("concurrent"), "{v}");
    hold.notify_one();
    let _ = running.text().await;
    // The first turn ended: its slot is free.
    w.chat.push(Turn::text(&["Now."]));
    let (s, frames) = sse(
        &w,
        &d.client,
        &format!("/chat/api/threads/{second}/send"),
        json!({ "content": "and another" }),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(error_codes(&frames), Vec::<String>::new(), "{frames:?}");
    let rows = rows_of(&w, d.id).await;
    assert!(
        rows.iter()
            .any(|r| r.2 == 429 && r.3.as_deref() == Some("key_rate")),
        "the refusal's row: {rows:?}"
    );
}

/// A turn a device has read aloud (`speak: true`) speaks as the device too
/// (review W3-11): the TTS calls are the device's rows, beside its chat row.
#[tokio::test]
async fn a_device_s_spoken_turn_s_speech_is_its_own() {
    let (w, d) = device_world().await;
    w.chat.push(Turn::text(&["Hello", " there."]));
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let (s, frames) = sse(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "hi", "speak": true }),
    )
    .await;
    assert_eq!(s, 200);
    assert!(frames.iter().any(|(e, _)| e == "done"), "{frames:?}");
    let rows = rows_of(&w, d.id).await;
    assert!(
        rows.iter().any(|r| r.1 == "chatty" && r.2 == 200),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.1 == "speak" && r.2 == 200),
        "the speech is the device's: {rows:?}"
    );
}

#[tokio::test]
async fn rpm_counts_each_model_call_and_expiry_refuses_at_the_route() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({ "rpm_limit": 1 })).await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    w.chat.push(Turn::text(&["One."]));
    let send = format!("/chat/api/threads/{tid}/send");
    let (_, first) = sse(&w, &d.client, &send, json!({ "content": "one" })).await;
    assert_eq!(error_codes(&first), Vec::<String>::new(), "{first:?}");
    // The routes themselves count nothing (a read is no model call) …
    let (s, _) = super::get(&w, &d.client, "/chat/api/threads").await;
    assert_eq!(s, 200);
    // … the second model call is the one over the limit.
    let (_, second) = sse(&w, &d.client, &send, json!({ "content": "two" })).await;
    assert_eq!(error_codes(&second), vec!["key_rate"], "{second:?}");
    assert_eq!(w.chat.seen.chat_count(), 1);

    // Past its expiry, the route itself refuses the device (L17).
    let (s, v) = op(
        &w,
        "key_set",
        json!({ "id": d.id, "expires_at": "2020-01-01T00:00:00Z" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &send, json!({ "content": "three" })).await;
    assert_eq!((s, &v["code"]), (401, &json!("key_expired")), "{v}");
}

#[tokio::test]
async fn a_device_s_tools_resolve_under_its_tool_scope() {
    let w = world(|_| {}).await;
    let d = pair(
        &w,
        "phone",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "pfx__*" }),
    )
    .await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    // The owner attaches a toolset outside the device's scope. (The
    // self-admin toolset would take the thread out of the device's reach
    // altogether: `self_admin`.)
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "docs" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let send = format!("/chat/api/threads/{tid}/send");

    // The device's turn: the label is reported as out of its reach, and with
    // no tool left the turn is refused before any model call.
    let (_, frames) = sse(&w, &d.client, &send, json!({ "content": "status?" })).await;
    let said: Vec<String> = frames
        .iter()
        .filter(|(e, _)| e == "error")
        .map(|(_, d)| d["message"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        said.iter()
            .any(|m| m.contains("'docs'") && m.contains("tool scope of device 'phone'")),
        "{said:?}"
    );
    assert_eq!(w.chat.seen.chat_count(), 0);

    // The owner's turn on the same thread reaches it.
    w.chat.push(Turn::text(&["All good."]));
    let (_, frames) = sse(&w, &w.gw.client(), &send, json!({ "content": "status?" })).await;
    assert!(frames.iter().any(|(e, _)| e == "done"), "{frames:?}");
    let tools = w.chat.seen.chat(0)["tools"].clone();
    assert!(
        tools.to_string().contains("docs__"),
        "the owner's turn offers the toolset: {tools}"
    );
}

#[tokio::test]
async fn a_device_out_of_the_knowledge_tools_is_not_searched_for() {
    let w = world(|_| {}).await;
    let kb = lmgw_core::knowledge::store::insert_kb(
        &w.state.knowledge.pool,
        &lmgw_core::knowledge::store::NewKb {
            name: "Taxes".into(),
            description: String::new(),
            embed_alias: "e".into(),
            embed: quickdoc_core::embed::EmbedIdentity::new("u", "m", 4),
            rerank_alias: String::new(),
            vision_alias: String::new(),
            chunk_tokens: 512,
            chunk_overlap: 64,
            mcp_visible: false,
        },
    )
    .await
    .unwrap();
    let d = pair(
        &w,
        "phone",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "docs__*" }),
    )
    .await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    // The owner gives the thread a base, in auto mode.
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "kb_ids": [kb], "kb_mode": "auto" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let send = format!("/chat/api/threads/{tid}/send");
    let retrieval = |frames: &[(String, Value)]| {
        frames
            .iter()
            .find(|(e, _)| e == "retrieval")
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| panic!("no retrieval in {frames:?}"))
    };
    w.chat.push(Turn::text(&["Not searched."]));
    let (_, frames) = sse(&w, &d.client, &send, json!({ "content": "what is due?" })).await;
    let r = retrieval(&frames);
    assert_eq!(r["searched"], json!([]), "{r}");
    assert!(r["notes"].to_string().contains("kb__search"), "{r}");
    // Not stored: the owner's own turn still searches.
    w.chat.push(Turn::text(&["Searched."]));
    let (_, frames) = sse(&w, &w.gw.client(), &send, json!({ "content": "and now?" })).await;
    let r = retrieval(&frames);
    assert!(r["notes"].to_string().contains("Taxes"), "{r}");
}

#[tokio::test]
async fn non_turn_routes_call_models_as_the_device_too() {
    let w = world(|_| {}).await;
    let d = pair(
        &w,
        "phone",
        json!({ "scope_mode": "allow", "scope_patterns": "chatty" }),
    )
    .await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let recording = wav(&speech(400), 16_000);

    // Dictation with the thread's ASR alias, which the scope keeps out.
    let resp = d
        .client
        .post(format!("{}/chat/api/threads/{tid}/transcribe", w.gw))
        .header("content-type", "audio/wav")
        .body(recording.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["code"], "key_scope", "{v}");

    // The press's warm: refused, nothing warmed.
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/voice/warm"),
        json!({ "stages": ["asr"] }),
    )
    .await;
    assert_eq!((s, &v["code"]), (403, &json!("key_scope")), "{v}");

    // Read-aloud of a stored reply with the thread's TTS alias.
    let reply = lmgw_core::store::ChatReply {
        content: "Sunny.".into(),
        ..Default::default()
    };
    let mid = lmgw_core::store::append_chat_reply(&w.state.db, tid, &reply)
        .await
        .unwrap();
    let (_, frames) = sse(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
        json!({}),
    )
    .await;
    let failed = frames
        .iter()
        .find(|(e, _)| e == "speech_error")
        .unwrap_or_else(|| panic!("no speech_error in {frames:?}"));
    assert_eq!(failed.1["code"], "key_scope", "{frames:?}");

    // An audio attachment's transcript at its upload.
    let resp = d
        .client
        .post(format!(
            "{}/chat/api/threads/{tid}/attachments?name=memo.wav",
            w.gw
        ))
        .body(recording.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let meta: Value = resp.json().await.unwrap();
    assert!(
        meta["meta"]["transcript_error"]
            .to_string()
            .contains("scope"),
        "{meta}"
    );
    // Every refusal is the device's row; nothing ran as internal:chat.
    let rows = rows_of(&w, d.id).await;
    assert!(rows.len() >= 3, "{rows:?}");
    assert!(rows.iter().all(|r| r.2 == 403), "{rows:?}");
    let internal: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_logs r JOIN api_keys k ON k.id = r.key_id \
             WHERE k.name = 'internal:chat'",
    )
    .fetch_one(&w.state.db)
    .await
    .unwrap();
    assert_eq!(internal, 0);

    // In scope, dictation is the device's call.
    let free = pair(&w, "tablet", json!({})).await;
    w.asr.push(Asr::Text("Wie spät ist es?"));
    let resp = free
        .client
        .post(format!("{}/chat/api/threads/{tid}/transcribe", w.gw))
        .header("content-type", "audio/wav")
        .body(recording)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        rows_of(&w, free.id).await,
        vec![("chat".into(), "hear".into(), 200, None)]
    );
}
