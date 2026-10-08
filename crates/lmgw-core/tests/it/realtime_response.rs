//! Responses over a real socket against a streaming chat fake (realtime
//! design §2.3, §4.3, §7.4, §7.5, §10.3, §16 "Session state machine, no
//! audio").
//!
//! The client side is replayed from the captured stock clients where one
//! covers the case (`openai_python.json` for a text turn, `agents_js_fc.json`
//! for a function call with its early follow-up), with one change for the
//! `@openai/agents` session: it asks for audio output, so its
//! `output_modalities` is set to text here (`realtime_speech` runs it
//! spoken).

use std::sync::Arc;

use lmgw_core::config::{KeyPolicy, ScopeMode};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::support::realtime_fakes::{
    captured_client_frames, chat_fake, events_until, gateway, next_event, open, send, text_session,
    types, user_text, Step, Turn, Ws, KEY,
};

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

/// The fields every delta-shaped event carries (§2.3: the SDK drops one
/// that lacks any of them).
fn assert_part_ref(ev: &Value) {
    for k in ["response_id", "item_id"] {
        assert!(ev[k].as_str().is_some_and(|s| !s.is_empty()), "{k} in {ev}");
    }
    for k in ["output_index", "content_index"] {
        assert!(ev[k].is_u64(), "{k} in {ev}");
    }
}

/// A user turn and a response to it, up to `response.done`.
async fn turn(ws: &mut Ws, text: &str, create: Value) -> Vec<Value> {
    send(ws, user_text(text)).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, create).await;
    events_until(ws, "response.done").await
}

#[tokio::test]
async fn a_text_turn_is_the_golden_sequence() {
    let fake = chat_fake().await;
    fake.push(Turn::text(&["Hal", "lo!"]));
    let (_s, addr) = gateway(&fake, false, None, |s| {
        s.realtime.default_model = "chatty".into();
    })
    .await;
    // openai-python 3.22.1, `client.realtime.connect(model="gpt-realtime")`.
    let mut ws = open(&addr, "/v1/realtime?model=gpt-realtime", &[]).await;
    let mut events = vec![next_event(&mut ws).await];
    for frame in captured_client_frames("openai_python.json") {
        let until = match frame["type"].as_str().unwrap() {
            "session.update" => "session.updated",
            "conversation.item.create" => "conversation.item.done",
            "response.create" => "response.done",
            other => panic!("unexpected captured frame {other}"),
        };
        send(&mut ws, frame).await;
        events.extend(events_until(&mut ws, until).await);
    }

    assert_eq!(
        types(&events),
        [
            "session.created",
            "session.updated",
            "conversation.item.added",
            "conversation.item.done",
            "response.created",
            "response.output_item.added",
            "conversation.item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "conversation.item.done",
            "response.done",
        ]
    );
    // Every event has its own server-minted id.
    let mut ids: Vec<&str> = events
        .iter()
        .map(|e| e["event_id"].as_str().expect("an event_id"))
        .collect();
    assert!(ids.iter().all(|i| i.starts_with("event_")));
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), events.len(), "event ids are unique");
    for e in &events {
        let t = e["type"].as_str().unwrap();
        if t.starts_with("response.content_part") || t.starts_with("response.output_text") {
            assert_part_ref(e);
        }
    }

    let user_id = events[2]["item"]["id"].as_str().unwrap();
    let created = &events[4]["response"];
    assert_eq!(created["object"], "realtime.response");
    assert_eq!(created["status"], "in_progress");
    assert_eq!(created["output"], json!([]));
    assert_eq!(created["output_modalities"], json!(["text"]));
    let resp_id = created["id"].as_str().unwrap();
    assert!(resp_id.starts_with("resp_"));

    let added = &events[5];
    assert_eq!(added["response_id"], resp_id);
    assert_eq!(added["output_index"], 0);
    assert_eq!(added["item"]["status"], "in_progress");
    assert_eq!(added["item"]["role"], "assistant");
    assert_eq!(added["item"]["content"], json!([]));
    assert_eq!(events[6]["previous_item_id"], user_id);
    assert_eq!(events[7]["part"], json!({"type": "text", "text": ""}));
    assert_eq!(events[8]["delta"], "Hal");
    assert_eq!(events[9]["delta"], "lo!");
    assert_eq!(events[10]["text"], "Hallo!");
    assert_eq!(
        events[11]["part"],
        json!({"type": "text", "text": "Hallo!"})
    );
    let item = &events[12]["item"];
    assert_eq!(item["status"], "completed");
    assert_eq!(
        item["content"],
        json!([{"type": "output_text", "text": "Hallo!"}])
    );
    assert_eq!(events[13]["item"], *item);
    assert_eq!(events[13]["previous_item_id"], user_id);

    let done = &events[14]["response"];
    assert_eq!(done["id"], resp_id);
    assert_eq!(done["status"], "completed");
    assert_eq!(done["status_details"], Value::Null);
    assert_eq!(done["output"], json!([item]));
    assert_eq!(
        done["usage"],
        json!({"total_tokens": 20, "input_tokens": 12, "output_tokens": 8,
               "input_token_details": {"text_tokens": 12, "audio_tokens": 0},
               "output_token_details": {"text_tokens": 8, "audio_tokens": 0}})
    );

    // What the chat model got: the transcript so far — no instructions, as
    // the session named none and the owner's default voice instructions are
    // for sessions that speak (package B review 8) — streamed, thinking off.
    let body = fake.seen.chat(0);
    assert_eq!(body["model"], "m");
    assert_eq!(body["stream"], true);
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": "Hallo"}])
    );
    assert_eq!(body["reasoning_effort"], "none");
    assert!(body.get("tools").is_none());
}

#[tokio::test]
async fn a_function_call_round_trip_with_the_early_follow_up() {
    let fake = chat_fake().await;
    let release = Arc::new(Notify::new());
    fake.push(Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id: Some("call_x1"),
            name: "get_time",
        },
        Step::CallArgs {
            index: 0,
            args: r#"{"tz":"#,
        },
        Step::CallArgs {
            index: 0,
            args: r#""Europe/Berlin"}"#,
        },
        Step::Finish("tool_calls"),
        // Generation is over; the stream's end is not. The client's
        // follow-up lands here, before `response.done`.
        Step::Wait(release.clone()),
        Step::Usage(6, 4),
    ]));
    fake.push(Turn::text(&["Es ist ", "12 Uhr."]));
    let (_s, addr) = gateway(&fake, false, None, |s| {
        s.realtime.default_model = "chatty".into();
    })
    .await;

    // `@openai/agents` with an explicit URL: no `?model=`.
    let mut ws = open(&addr, "/v1/realtime", &[]).await;
    next_event(&mut ws).await;
    let mut frames = captured_client_frames("agents_js_fc.json").into_iter();
    let mut first = frames.next().unwrap();
    first["session"]["output_modalities"] = json!(["text"]);
    send(&mut ws, first).await;
    let updated = next_event(&mut ws).await;
    // The session keeps the tool as the client wrote it.
    assert_eq!(
        updated["session"]["tools"][0]["parameters"]["$schema"],
        "http://json-schema.org/draft-07/schema#"
    );
    send(&mut ws, frames.next().unwrap()).await; // {type, tracing}
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    send(&mut ws, frames.next().unwrap()).await; // the user's "Hallo"
    events_until(&mut ws, "conversation.item.done").await;
    let create_1 = frames.next().unwrap();
    assert_eq!(create_1["event_id"], "agents_js_response_create_1");
    send(&mut ws, create_1).await;

    let mut events = events_until(&mut ws, "conversation.item.done").await;
    assert_eq!(
        types(&events),
        [
            "response.created",
            "response.output_item.added",
            "conversation.item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "conversation.item.done",
        ]
    );
    let call = &events[1]["item"];
    assert_eq!(call["type"], "function_call");
    assert_eq!(call["status"], "in_progress");
    assert_eq!(call["arguments"], "");
    assert_eq!(call["name"], "get_time");
    // The upstream's id, new to this session, is kept.
    assert_eq!(call["call_id"], "call_x1");
    for d in &events[3..5] {
        assert_eq!(d["call_id"], "call_x1");
        assert!(d["item_id"].is_string() && d["response_id"].is_string());
        assert!(d["output_index"].is_u64());
    }
    assert_eq!(events[5]["name"], "get_time");
    assert_eq!(events[5]["arguments"], r#"{"tz":"Europe/Berlin"}"#);
    assert_eq!(events[6]["item"]["status"], "completed");
    assert_eq!(events[6]["item"]["arguments"], r#"{"tz":"Europe/Berlin"}"#);

    // The client answers at once, as the stock client does (~13 ms after
    // `output_item.done`): its output and the follow-up `response.create`.
    let output = frames.next().unwrap();
    assert_eq!(output["item"]["call_id"], "call_x1");
    send(&mut ws, output).await;
    let create_2 = frames.next().unwrap();
    assert_eq!(create_2["event_id"], "agents_js_response_create_2");
    send(&mut ws, create_2).await;
    events.extend(events_until(&mut ws, "conversation.item.done").await);
    // Queued, not refused: let the first response end.
    release.notify_one();
    events.extend(events_until(&mut ws, "response.done").await);
    let first_done = events.last().unwrap()["response"].clone();
    assert_eq!(first_done["status"], "completed");
    assert_eq!(first_done["output"][0]["type"], "function_call");
    assert_eq!(first_done["usage"]["total_tokens"], 10);
    events.extend(events_until(&mut ws, "response.done").await);

    assert!(
        !events.iter().any(|e| e["type"] == "error"),
        "no event was refused: {events:?}"
    );
    // The stock client runs a tool on every completed output item event:
    // exactly one says so for this call.
    let completed = events
        .iter()
        .filter(|e| {
            e["type"]
                .as_str()
                .unwrap()
                .starts_with("response.output_item")
                && e["item"]["call_id"] == "call_x1"
                && e["item"]["status"] == "completed"
        })
        .count();
    assert_eq!(completed, 1);
    let second_done = &events.last().unwrap()["response"];
    assert_eq!(second_done["status"], "completed");
    assert_eq!(
        second_done["output"][0]["content"][0]["text"],
        "Es ist 12 Uhr."
    );
    assert_ne!(second_done["id"], first_done["id"]);

    // Upstream: the tool without `$schema`, and the result right after the
    // call it answers.
    let body = fake.seen.chat(0);
    let params = &body["tools"][0]["function"]["parameters"];
    assert!(params.get("$schema").is_none(), "{params}");
    assert_eq!(
        params["properties"]["tz"]["anyOf"][1],
        json!({"type": "null"})
    );
    assert_eq!(body["tool_choice"], "auto");
    let body = fake.seen.chat(1);
    let roles: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool"]);
    assert_eq!(
        body["messages"][0]["content"],
        "You are a helpful voice assistant."
    );
    let tool_call = &body["messages"][2]["tool_calls"][0];
    assert_eq!(tool_call["id"], "call_x1");
    assert_eq!(tool_call["function"]["name"], "get_time");
    assert_eq!(
        serde_json::from_str::<Value>(tool_call["function"]["arguments"].as_str().unwrap())
            .unwrap(),
        json!({"tz": "Europe/Berlin"})
    );
    assert_eq!(body["messages"][3]["tool_call_id"], "call_x1");
    assert_eq!(body["messages"][3]["content"], "12:00");
}

/// One call turn: the model calls `f` with the upstream id `id`.
fn call_turn(id: Option<&'static str>) -> Turn {
    Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id,
            name: "f",
        },
        Step::CallArgs {
            index: 0,
            args: "{}",
        },
        Step::Finish("tool_calls"),
        Step::Usage(5, 1),
    ])
}

#[tokio::test]
async fn call_ids_are_session_unique_when_the_upstream_repeats_one() {
    let fake = chat_fake().await;
    // Local models number their calls per request; one without an id gets
    // `call_0` from the decoder, which repeats too.
    fake.push(call_turn(Some("call_0")));
    fake.push(call_turn(Some("call_0")));
    fake.push(call_turn(None));
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;

    let mut seen_ids = Vec::new();
    send(&mut ws, user_text("los")).await;
    events_until(&mut ws, "conversation.item.done").await;
    for _ in 0..3 {
        send(&mut ws, json!({"type": "response.create"})).await;
        let events = events_until(&mut ws, "response.done").await;
        let call_id = events.last().unwrap()["response"]["output"][0]["call_id"]
            .as_str()
            .unwrap()
            .to_string();
        send(
            &mut ws,
            json!({"type": "conversation.item.create",
                   "item": {"type": "function_call_output", "call_id": call_id,
                            "output": "done"}}),
        )
        .await;
        events_until(&mut ws, "conversation.item.done").await;
        seen_ids.push(call_id);
    }
    assert_eq!(seen_ids[0], "call_0", "a new upstream id is kept");
    for id in &seen_ids[1..] {
        assert!(id.starts_with("call_") && id != "call_0", "{id}");
    }
    assert_ne!(seen_ids[1], seen_ids[2]);

    // The next rendering carries each call once, each with its own result.
    send(&mut ws, json!({"type": "response.create"})).await;
    events_until(&mut ws, "response.done").await;
    let body = fake.seen.chat(3);
    let calls: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["tool_calls"][0]["id"].as_str())
        .collect();
    assert_eq!(
        calls,
        seen_ids.iter().map(String::as_str).collect::<Vec<_>>()
    );
    let results: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["tool_call_id"].as_str())
        .collect();
    assert_eq!(results, calls);
}

#[tokio::test]
async fn a_second_create_while_generating_is_refused_with_its_event_id() {
    let fake = chat_fake().await;
    let release = Arc::new(Notify::new());
    fake.push(Turn::Stream(vec![
        Step::Text("eins"),
        Step::Wait(release.clone()),
        Step::Finish("stop"),
        Step::Usage(3, 1),
    ]));
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "agents_js_response_create_1"}),
    )
    .await;
    events_until(&mut ws, "response.output_text.delta").await;

    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "agents_js_response_create_2"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error");
    assert_eq!(code(&e), "conversation_already_has_active_response");
    assert_eq!(e["error"]["event_id"], "agents_js_response_create_2");

    // The active one is untouched by it.
    release.notify_one();
    let done = events_until(&mut ws, "response.done").await;
    assert_eq!(done.last().unwrap()["response"]["status"], "completed");
    assert_eq!(fake.seen.chat_count(), 1);
}

#[tokio::test]
async fn per_call_rate_limits_count_model_calls_inside_the_session_s_slot() {
    let fake = chat_fake().await;
    // One concurrent request — the session — and three a minute: three model
    // calls, because the handshake is not a model call and is not counted
    // (§10.3).
    let (_s, addr) = gateway(
        &fake,
        true,
        Some(KeyPolicy {
            rpm_limit: 3,
            concurrency_limit: 1,
            ..Default::default()
        }),
        |_| {},
    )
    .await;
    let bearer = format!("Bearer {KEY}");
    let mut ws = open(
        &addr,
        "/v1/realtime?model=chatty",
        &[("authorization", &bearer)],
    )
    .await;
    next_event(&mut ws).await;
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    next_event(&mut ws).await;

    for n in 0..3 {
        let events = turn(&mut ws, "hi", json!({"type": "response.create"})).await;
        assert_eq!(
            events.last().unwrap()["response"]["status"],
            "completed",
            "call {n}: {events:?}"
        );
    }
    let events = turn(
        &mut ws,
        "hi",
        json!({"type": "response.create", "event_id": "r3"}),
    )
    .await;
    assert_eq!(
        types(&events),
        ["response.created", "error", "response.done"]
    );
    assert_eq!(code(&events[1]), "key_rate");
    assert_eq!(events[1]["error"]["event_id"], "r3");
    let r = &events[2]["response"];
    assert_eq!(r["status"], "failed");
    assert_eq!(r["status_details"]["type"], "failed");
    assert_eq!(r["status_details"]["error"]["code"], "key_rate");
    assert_eq!(fake.seen.chat_count(), 3, "the refused call was never made");
}

#[tokio::test]
async fn the_key_s_scope_is_checked_again_before_every_call() {
    let fake = chat_fake().await;
    let (state, addr) = gateway(
        &fake,
        true,
        Some(KeyPolicy {
            scope_mode: ScopeMode::Allow,
            scope_patterns: "chatty".into(),
            ..Default::default()
        }),
        |_| {},
    )
    .await;
    let bearer = format!("Bearer {KEY}");
    let mut ws = open(
        &addr,
        "/v1/realtime?model=chatty",
        &[("authorization", &bearer)],
    )
    .await;
    next_event(&mut ws).await;
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    next_event(&mut ws).await;
    let events = turn(&mut ws, "hi", json!({"type": "response.create"})).await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");

    // The owner narrows the key while the session is open.
    sqlx::query("UPDATE api_keys SET scope_patterns = 'other' WHERE name = 'voice'")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let events = turn(
        &mut ws,
        "again",
        json!({"type": "response.create", "event_id": "r2"}),
    )
    .await;
    assert_eq!(code(&events[1]), "key_scope", "{events:?}");
    assert_eq!(events[1]["error"]["event_id"], "r2");
    assert_eq!(events[2]["response"]["status"], "failed");

    // The refusal is traffic: it has its row, labelled like the session's.
    let (refusals,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM request_logs WHERE error_kind = 'key_scope' AND requested_alias = \
         'chatty' AND ingress_proto = 'realtime'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(refusals, 1);
}

#[tokio::test]
async fn a_key_renamed_mid_session_keeps_its_scope_and_budget() {
    // WP2 review R7: the per-call check found the key by id but read its
    // scope and budget by the name the handshake captured — a renamed key
    // then matched no row and was checked against nothing.
    let fake = chat_fake().await;
    let (state, addr) = gateway(
        &fake,
        true,
        Some(KeyPolicy {
            scope_mode: ScopeMode::Allow,
            scope_patterns: "chatty".into(),
            ..Default::default()
        }),
        |_| {},
    )
    .await;
    let bearer = format!("Bearer {KEY}");
    let mut ws = open(
        &addr,
        "/v1/realtime?model=chatty",
        &[("authorization", &bearer)],
    )
    .await;
    next_event(&mut ws).await;
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    next_event(&mut ws).await;
    let events = turn(&mut ws, "hi", json!({"type": "response.create"})).await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");

    // Renamed, and narrowed under its new name.
    sqlx::query(
        "UPDATE api_keys SET name = 'voice-renamed', scope_patterns = 'other' WHERE name = \
         'voice'",
    )
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let events = turn(
        &mut ws,
        "again",
        json!({"type": "response.create", "event_id": "r2"}),
    )
    .await;
    assert_eq!(code(&events[1]), "key_scope", "{events:?}");
    assert_eq!(events[2]["response"]["status"], "failed");
    assert_eq!(fake.seen.chat_count(), 1, "the refused call was never made");
}

#[tokio::test]
async fn a_key_renamed_mid_session_is_charged_by_its_identity() {
    // Package A review #2: every row resolved its key by the name the
    // handshake captured — after a rename its `key_id` was NULL, so the
    // key's budget and tokens/minute never saw the session's calls, and a
    // new key created under the old name was charged instead.
    let fake = chat_fake().await;
    let (state, addr) = gateway(
        &fake,
        true,
        Some(KeyPolicy {
            tpm_limit: 1,
            ..Default::default()
        }),
        |_| {},
    )
    .await;
    sqlx::query(
        "INSERT INTO prices (scope_kind, scope_key, price_in, price_out) VALUES ('alias', \
         'chatty', 1000.0, 1000.0)",
    )
    .execute(&state.db)
    .await
    .unwrap();
    sqlx::query("UPDATE api_keys SET budget_micro = 1 WHERE name = 'voice'")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let (id,): (i64,) = sqlx::query_as("SELECT id FROM api_keys WHERE name = 'voice'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let bearer = format!("Bearer {KEY}");
    let mut ws = open(
        &addr,
        "/v1/realtime?model=chatty",
        &[("authorization", &bearer)],
    )
    .await;
    next_event(&mut ws).await;
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    next_event(&mut ws).await;

    // Renamed while the session is open, and a new key takes the old name.
    sqlx::query("UPDATE api_keys SET name = 'voice-renamed' WHERE id = ?1")
        .bind(id)
        .execute(&state.db)
        .await
        .unwrap();
    const OTHER: &str = "lmgw-realtime-other-key";
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, budget_micro, tpm_limit)
         VALUES ('voice', ?1, 1, 1, 1)",
    )
    .bind(lmgw_core::config::hash_api_key(OTHER))
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let (newcomer,): (i64,) = sqlx::query_as("SELECT id FROM api_keys WHERE name = 'voice'")
        .fetch_one(&state.db)
        .await
        .unwrap();

    let events = turn(&mut ws, "hi", json!({"type": "response.create"})).await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    // The row carries the key's new name and its id.
    let rows: Vec<(Option<String>, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT client_key, key_id, cost_micro FROM request_logs WHERE ingress_proto = \
         'realtime' ORDER BY id",
    )
    .fetch_all(&state.db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0.as_deref(), Some("voice-renamed"));
    assert_eq!(rows[0].1, Some(id));
    assert!(rows[0].2.is_some_and(|c| c > 1), "{rows:?}");

    // Its spend reached the key's budget of one micro-unit...
    let events = turn(&mut ws, "again", json!({"type": "response.create"})).await;
    assert_eq!(code(&events[1]), "key_budget", "{events:?}");
    // ...and its tokens the key's tokens/minute, once the budget is lifted.
    sqlx::query("UPDATE api_keys SET budget_micro = 0 WHERE id = ?1")
        .bind(id)
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let events = turn(&mut ws, "once more", json!({"type": "response.create"})).await;
    assert_eq!(code(&events[1]), "key_rate", "{events:?}");
    assert!(events[1]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("tokens/minute"));
    assert_eq!(fake.seen.chat_count(), 1);
    // The refusals are the renamed key's rows too.
    let (theirs,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM request_logs WHERE key_id = ?1 AND client_key = 'voice-renamed'",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(theirs, 3);

    // The newcomer under the old name was charged nothing: no rows, and its
    // own budget and tokens/minute of one still let a session in.
    let (charged,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM request_logs WHERE key_id = ?1")
        .bind(newcomer)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(charged, 0);
    let other = format!("Bearer {OTHER}");
    let mut ws2 = open(
        &addr,
        "/v1/realtime?model=chatty",
        &[("authorization", &other)],
    )
    .await;
    assert_eq!(next_event(&mut ws2).await["type"], "session.created");
}

#[tokio::test]
async fn a_key_disabled_mid_session_ends_it_with_the_reason() {
    let fake = chat_fake().await;
    let (state, addr) = gateway(&fake, true, Some(KeyPolicy::default()), |_| {}).await;
    let bearer = format!("Bearer {KEY}");
    let mut ws = open(
        &addr,
        "/v1/realtime?model=chatty",
        &[("authorization", &bearer)],
    )
    .await;
    next_event(&mut ws).await;
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    next_event(&mut ws).await;
    let events = turn(&mut ws, "hi", json!({"type": "response.create"})).await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");

    // Disabled while the session is open — by any path, here the table
    // itself: the published snapshot re-arms every revocation watch, and the
    // session ends now, saying why (client-apps design §1.6, changed
    // 2026-10-06: Disable means disable, for every key). Before, the socket
    // stayed and only its next call was refused; that per-call check stays
    // the backstop (`proxy::policy_checked_call`'s own tests).
    sqlx::query("UPDATE api_keys SET enabled = 0 WHERE name = 'voice'")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let close = loop {
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::Message;
        match tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Close(c)))) => break c,
            Ok(Some(Ok(_))) => continue,
            other => panic!("expected the close, got {other:?}"),
        }
    };
    let close = close.expect("a close frame with a reason");
    assert_eq!(u16::from(close.code), 4003);
    assert_eq!(close.reason.as_str(), "revoked: key 'voice' was disabled");
    assert_eq!(
        fake.seen.chat_count(),
        1,
        "no call after the disable reached the model"
    );
}

#[tokio::test]
async fn a_context_overflow_fails_the_response_and_the_session_goes_on() {
    let fake = chat_fake().await;
    // llama-server's own refusal, which the gateway maps to the stable code.
    fake.push(Turn::Status(
        400,
        json!({"error": {"code": 400, "type": "exceed_context_size_error",
                         "message": "the request exceeds the available context size",
                         "n_prompt_tokens": 5000, "n_ctx": 4096}}),
    ));
    let (state, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    let events = turn(
        &mut ws,
        "a long story",
        json!({"type": "response.create", "event_id": "big"}),
    )
    .await;
    assert_eq!(
        types(&events),
        ["response.created", "error", "response.done"]
    );
    assert_eq!(code(&events[1]), "context_length_exceeded");
    assert_eq!(events[1]["error"]["event_id"], "big");
    assert!(events[1]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("4096"));
    let r = &events[2]["response"];
    assert_eq!(r["status"], "failed");
    assert_eq!(
        r["status_details"]["error"]["code"],
        "context_length_exceeded"
    );
    assert_eq!(r["usage"], Value::Null);

    // The client can carry on (and could delete items first).
    let events = turn(&mut ws, "kürzer", json!({"type": "response.create"})).await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");

    // Both calls wrote their rows, under the realtime label (§11).
    let (rows,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM request_logs WHERE requested_alias = 'chatty' AND ingress_proto = \
         'realtime'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(rows, 2);
}

#[tokio::test]
async fn a_response_echoes_its_metadata_and_takes_its_overrides_for_itself_only() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "instructions": "Session instructions."}}),
    )
    .await;
    next_event(&mut ws).await;

    let events = turn(
        &mut ws,
        "hi",
        json!({"type": "response.create", "response": {
            "metadata": {"turn": 3, "client": "talk"},
            "instructions": "Answer in German.",
            "max_output_tokens": 50,
            // A tool without `type` is a function tool (GA default).
            "tools": [{"name": "g", "parameters": {"type": "object", "properties": {}}}],
            "tool_choice": {"type": "function", "name": "g"}
        }}),
    )
    .await;
    let meta = json!({"turn": 3, "client": "talk"});
    assert_eq!(events[0]["response"]["metadata"], meta);
    assert_eq!(events.last().unwrap()["response"]["metadata"], meta);
    let body = fake.seen.chat(0);
    assert_eq!(body["messages"][0]["content"], "Answer in German.");
    assert_eq!(body["max_tokens"], 50);
    assert_eq!(body["tools"][0]["function"]["name"], "g");
    assert_eq!(body["tool_choice"]["function"]["name"], "g");

    // The next response is the session's again.
    let events = turn(&mut ws, "and now?", json!({"type": "response.create"})).await;
    assert_eq!(events[0]["response"]["metadata"], Value::Null);
    let body = fake.seen.chat(1);
    assert_eq!(body["messages"][0]["content"], "Session instructions.");
    assert!(body.get("max_tokens").is_none());
    assert!(body.get("tools").is_none());
}

#[tokio::test]
async fn responses_this_build_cannot_make_are_refused_before_they_start() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;

    for (response, want, param) in [
        (
            json!({"conversation": "none"}),
            "unsupported",
            "response.conversation",
        ),
        (
            json!({"input": [{"type": "message", "role": "user", "content": []}]}),
            "unsupported",
            "response.input",
        ),
        (
            json!({"output_modalities": ["audio"]}),
            "tts_not_configured",
            "session.lmgw.tts_model",
        ),
    ] {
        send(
            &mut ws,
            json!({"type": "response.create", "event_id": want, "response": response}),
        )
        .await;
        let e = next_event(&mut ws).await;
        assert_eq!(e["type"], "error", "{e}");
        assert_eq!(code(&e), want);
        assert_eq!(e["error"]["param"], param);
        assert_eq!(e["error"]["event_id"], want);
    }
    assert_eq!(fake.seen.chat_count(), 0);

    // A session with no chat model at all says which setting is missing.
    let mut ws = open(&addr, "/v1/realtime", &[]).await;
    next_event(&mut ws).await;
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "x",
               "response": {"output_modalities": ["text"]}}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(code(&e), "chat_not_configured");
    assert!(e["error"]["message"]
        .as_str()
        .unwrap()
        .contains("realtime.default_model"));
}

#[tokio::test]
async fn an_item_the_active_response_is_producing_cannot_be_deleted() {
    let fake = chat_fake().await;
    let release = Arc::new(Notify::new());
    fake.push(Turn::Stream(vec![
        Step::Text("eins"),
        Step::Wait(release.clone()),
        Step::Finish("stop"),
    ]));
    let (_s, addr) = gateway(&fake, false, None, |_| {}).await;
    let mut ws = text_session(&addr).await;
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.output_text.delta").await;
    let item_id = events[1]["item"]["id"].clone();
    send(
        &mut ws,
        json!({"type": "conversation.item.delete", "event_id": "d1", "item_id": item_id}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(code(&e), "invalid_value");
    assert_eq!(e["error"]["event_id"], "d1");
    release.notify_one();
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    // Once done, it can go.
    send(
        &mut ws,
        json!({"type": "conversation.item.delete", "item_id": item_id}),
    )
    .await;
    assert_eq!(
        next_event(&mut ws).await["type"],
        "conversation.item.deleted"
    );
}
