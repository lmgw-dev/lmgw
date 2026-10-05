//! The conversation's client events over a real socket (realtime design
//! §7.1, §7.3, §16): `conversation.item.create` with and without an id and
//! with each `previous_item_id` form, `.retrieve`, `.delete`, `.truncate`,
//! and the refusals — each one `error` echoing the client's `event_id`, the
//! session going on.

use serde_json::{json, Value};

use crate::support::realtime_fakes::{
    captured_client_frames, chat_fake, gateway, next_event, send, text_session, user_text, Ws,
};

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

/// Create `event` and return its `conversation.item.added` and `.done`.
async fn created(ws: &mut Ws, event: Value) -> (Value, Value) {
    send(ws, event).await;
    let added = next_event(ws).await;
    assert_eq!(added["type"], "conversation.item.added", "{added}");
    let done = next_event(ws).await;
    assert_eq!(done["type"], "conversation.item.done", "{done}");
    (added, done)
}

/// One request, expected to be refused: its `error`.
async fn refused(ws: &mut Ws, event: Value) -> Value {
    send(ws, event).await;
    let e = next_event(ws).await;
    assert_eq!(e["type"], "error", "{e}");
    e
}

#[tokio::test]
async fn a_client_item_gets_an_id_and_lands_where_it_was_asked_to() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;

    // The Python SDK's own frame: no event_id, no item id (openai_python.json).
    let frame = captured_client_frames("openai_python.json")
        .into_iter()
        .find(|f| f["type"] == "conversation.item.create")
        .unwrap();
    let (added, done) = created(&mut ws, frame).await;
    let first = added["item"]["id"].as_str().unwrap().to_string();
    assert!(first.starts_with("item_"), "{first}");
    assert_eq!(added["previous_item_id"], Value::Null);
    assert_eq!(added["item"]["object"], "realtime.item");
    assert_eq!(added["item"]["status"], "completed");
    assert_eq!(added["item"]["content"][0]["text"], "Hallo");
    assert_eq!(done["item"], added["item"]);
    assert!(added["event_id"].as_str().unwrap().starts_with("event_"));

    // Appended: after the first.
    let (second, _) = created(&mut ws, user_text("zwei")).await;
    assert_eq!(second["previous_item_id"], first.as_str());
    let second = second["item"]["id"].as_str().unwrap().to_string();

    // `root`: at the start.
    let mut ev = user_text("null");
    ev["previous_item_id"] = json!("root");
    ev["item"]["id"] = json!("my_own_id");
    let (zero, _) = created(&mut ws, ev).await;
    assert_eq!(zero["previous_item_id"], Value::Null);
    assert_eq!(zero["item"]["id"], "my_own_id");

    // After a named item: between the first and the second.
    let mut ev = user_text("eins-b");
    ev["previous_item_id"] = json!(first);
    let (between, _) = created(&mut ws, ev).await;
    assert_eq!(between["previous_item_id"], first.as_str());
    let between = between["item"]["id"].as_str().unwrap().to_string();

    // The order is visible in what the model gets.
    send(
        &mut ws,
        json!({"type": "response.create", "response": {"metadata": {"n": 1}}}),
    )
    .await;
    crate::support::realtime_fakes::events_until(&mut ws, "response.done").await;
    // (A text session: no default voice instructions before them.)
    let body = fake.seen.chat(0);
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(body["messages"][0]["role"], "user");
    let texts: Vec<&str> = body["messages"][0]["content"]
        .as_array()
        .map(|parts| parts.iter().map(|p| p["text"].as_str().unwrap()).collect())
        .unwrap_or_else(|| vec![body["messages"][0]["content"].as_str().unwrap()]);
    assert_eq!(texts.join("\n"), "null\nHallo\neins-b\nzwei");

    // Retrieve, delete, and the item is gone.
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "event_id": "r1", "item_id": between}),
    )
    .await;
    let got = next_event(&mut ws).await;
    assert_eq!(got["type"], "conversation.item.retrieved");
    assert_eq!(got["item"]["content"][0]["text"], "eins-b");

    send(
        &mut ws,
        json!({"type": "conversation.item.delete", "item_id": between}),
    )
    .await;
    let deleted = next_event(&mut ws).await;
    assert_eq!(deleted["type"], "conversation.item.deleted");
    assert_eq!(deleted["item_id"], between.as_str());

    let e = refused(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "event_id": "r2", "item_id": between}),
    )
    .await;
    assert_eq!(code(&e), "item_not_found");
    assert_eq!(e["error"]["event_id"], "r2");
    assert_eq!(e["error"]["param"], "item_id");

    // The next item follows what is now last, the model's answer.
    let (after, _) = created(&mut ws, user_text("drei")).await;
    assert_ne!(after["previous_item_id"], between.as_str());
    assert_ne!(after["previous_item_id"], second.as_str());
}

#[tokio::test]
async fn what_a_client_may_not_create_is_refused_by_name() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;

    let (first, _) = created(&mut ws, user_text("hi")).await;
    let id = first["item"]["id"].clone();

    // The same id twice.
    let mut ev = user_text("again");
    ev["event_id"] = json!("c1");
    ev["item"]["id"] = id;
    let e = refused(&mut ws, ev).await;
    assert_eq!(code(&e), "invalid_value");
    assert_eq!(e["error"]["event_id"], "c1");

    // An assistant cannot say input_text.
    let e = refused(
        &mut ws,
        json!({"type": "conversation.item.create", "event_id": "c2",
               "item": {"type": "message", "role": "assistant",
                        "content": [{"type": "input_text", "text": "x"}]}}),
    )
    .await;
    assert_eq!(code(&e), "invalid_value");
    assert_eq!(e["error"]["param"], "item.content[0]");
    assert_eq!(e["error"]["event_id"], "c2");

    // A function call must name its call_id.
    let e = refused(
        &mut ws,
        json!({"type": "conversation.item.create",
               "item": {"type": "function_call", "name": "f", "arguments": "{}"}}),
    )
    .await;
    assert_eq!(code(&e), "missing_required_parameter");

    // A call_id is session-unique.
    let call = json!({"type": "conversation.item.create",
                      "item": {"type": "function_call", "call_id": "call_mine", "name": "f",
                               "arguments": "{}"}});
    let (call_item, _) = created(&mut ws, call.clone()).await;
    let e = refused(&mut ws, call).await;
    assert_eq!(code(&e), "invalid_value");
    assert_eq!(e["error"]["param"], "item.call_id");

    // Audio with no transcript needs the ASR alias, which arrives later.
    let e = refused(
        &mut ws,
        json!({"type": "conversation.item.create",
               "item": {"type": "message", "role": "user",
                        "content": [{"type": "input_audio", "audio": "AAAA"}]}}),
    )
    .await;
    assert_eq!(code(&e), "not_implemented_yet");

    // A previous_item_id that is not there.
    let mut ev = user_text("x");
    ev["previous_item_id"] = json!("item_nowhere");
    let e = refused(&mut ws, ev).await;
    assert_eq!(code(&e), "item_not_found");
    assert_eq!(e["error"]["param"], "previous_item_id");

    // None of the refusals changed anything, and the session goes on.
    let (next, _) = created(&mut ws, user_text("still here")).await;
    assert_eq!(next["previous_item_id"], call_item["item"]["id"]);
}

#[tokio::test]
async fn truncate_says_what_it_cannot_cut() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    let truncate = |id: &Value, event_id: &str| {
        json!({"type": "conversation.item.truncate", "event_id": event_id, "item_id": id,
               "content_index": 0, "audio_end_ms": 1500})
    };

    let e = refused(&mut ws, truncate(&json!("item_nowhere"), "t1")).await;
    assert_eq!(code(&e), "item_not_found");
    assert_eq!(e["error"]["event_id"], "t1");

    // A text item has no audio to cut.
    let (text, _) = created(&mut ws, user_text("hi")).await;
    let e = refused(&mut ws, truncate(&text["item"]["id"], "t2")).await;
    assert_eq!(code(&e), "invalid_value");
    assert!(e["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no assistant audio"));

    // An assistant audio item the client made carries no audio: a cut at
    // 1500 ms is beyond it, as with OpenAI.
    let (audio, _) = created(
        &mut ws,
        json!({"type": "conversation.item.create",
               "item": {"type": "message", "role": "assistant",
                        "content": [{"type": "output_audio", "transcript": "Hallo"}]}}),
    )
    .await;
    let e = refused(&mut ws, truncate(&audio["item"]["id"], "t3")).await;
    assert_eq!(code(&e), "invalid_value");
    assert_eq!(e["error"]["param"], "audio_end_ms");
    assert_eq!(e["error"]["event_id"], "t3");
}
