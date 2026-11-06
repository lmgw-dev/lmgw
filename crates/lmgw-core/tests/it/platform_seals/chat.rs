//! The Chat's store and replay of the seals (§7.3.2, WP8 step 3): a plain
//! reply's in `chat_messages.reasoning_seals` (migration 0096), a tool
//! turn's inside its stored IR, each sent back to the row on the next turn
//! before what followed it.

use std::time::Duration;

use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::Request;

use super::{answer, platform, posted, reasoning_items, SEALED};
use crate::chat_actions::post;
use crate::chat_golden::{mcp_stub, register_stub, thread};
use crate::support::responses_wire as rw;

async fn send(gw: &crate::common::Gw, tid: i64, content: &str) {
    post(
        gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": content}),
    )
    .await
    .text()
    .await
    .unwrap();
}

#[tokio::test]
async fn a_plain_reply_s_seals_are_stored_and_go_back_before_its_text() {
    let (mock, state, gw, up) = platform(|req: &Request| {
        answer(
            req,
            &[
                rw::reasoning("rs_1", "Weighing it."),
                rw::message("msg_1", "Hello"),
            ],
        )
    })
    .await;
    let tid = thread(&gw, json!({"model_alias": "my-model"})).await;
    send(&gw, tid, "hi").await;
    let reply = store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(reply.reasoning, "Weighing it.");
    assert_eq!(
        reply.reasoning_seals,
        [store::ReplySeal::new(0, up.seal("rs_1", SEALED))]
    );
    assert!(reply.ir_messages.is_none(), "a plain turn has no record");
    // The page never sees the encrypted reasoning.
    let page = post(
        &gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "again"}),
    )
    .await
    .text()
    .await
    .unwrap();
    assert!(!page.contains(SEALED), "{page}");

    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 2);
    let input = sent[1]["input"].as_array().unwrap();
    let at = input
        .iter()
        .position(|i| i["type"] == "reasoning")
        .expect("the seal went back");
    assert_eq!(
        input[at],
        json!({"type": "reasoning", "id": "rs_1", "encrypted_content": SEALED,
               "summary": [{"type": "summary_text", "text": "Weighing it."}]})
    );
    assert_eq!(
        input[at + 1],
        json!({"role": "assistant", "content": "Hello"})
    );
}

#[tokio::test]
async fn an_edit_clears_a_reply_s_seals_with_its_reasoning() {
    let (_mock, state, gw, _up) = platform(|req: &Request| {
        answer(
            req,
            &[rw::reasoning("rs_1", "Hm."), rw::message("msg_1", "Hello")],
        )
    })
    .await;
    let tid = thread(&gw, json!({"model_alias": "my-model"})).await;
    send(&gw, tid, "hi").await;
    let reply = store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(reply.reasoning_seals.len(), 1);
    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{}/edit", reply.id),
        json!({"content": "Edited."}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let edited = store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(edited.reasoning, "");
    assert!(edited.reasoning_seals.is_empty());
}

/// A tool turn: the seal of the reasoning before the call goes back with
/// the call inside the loop, and again on the thread's next turn from the
/// stored record.
#[tokio::test]
async fn a_tool_turn_s_seals_ride_in_its_record() {
    let (mock, state, gw, up) = platform(|req: &Request| {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        match rw::offers(&body, "stub__echo")
            && !rw::after_tool(&body)
            && reasoning_items(&body).is_empty()
        {
            true => answer(
                req,
                &[
                    rw::reasoning("rs_1", "Use the tool."),
                    rw::call("fc_1", "c1", "stub__echo", "{\"text\":\"hi\"}"),
                ],
            ),
            false => answer(req, &[rw::message("msg_2", "done")]),
        }
    })
    .await;
    register_stub(&state, &mcp_stub(Duration::ZERO).await).await;
    let tid = thread(&gw, json!({"model_alias": "my-model"})).await;
    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "stub"}]}),
    )
    .await;
    assert_eq!(r.status(), 200);
    send(&gw, tid, "say hi through the tool").await;
    send(&gw, tid, "and now?").await;

    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3, "{sent:?}");
    for (n, body) in sent.iter().enumerate().skip(1) {
        let input = body["input"].as_array().unwrap();
        let at = input
            .iter()
            .position(|i| i["type"] == "reasoning")
            .unwrap_or_else(|| panic!("send {n}: {input:?}"));
        assert_eq!(input[at]["id"], "rs_1");
        assert_eq!(input[at]["encrypted_content"], SEALED);
        assert_eq!(
            input[at + 1]["type"],
            "function_call",
            "send {n}: {input:?}"
        );
        assert_eq!(input[at + 1]["call_id"], "c1");
    }
    let reply = store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.role == "assistant")
        .unwrap();
    assert!(
        reply.reasoning_seals.is_empty(),
        "a tool turn keeps them in its record"
    );
    let record: Vec<lmgw_core::ir::Message> =
        serde_json::from_str(reply.ir_messages.as_deref().unwrap()).unwrap();
    assert!(record
        .iter()
        .flat_map(|m| &m.content)
        .any(|p| p.seal() == Some(&up.seal("rs_1", SEALED))));
}

/// The input items of a Responses body, each as its id, or `<role>:<text>`
/// for a message.
fn shape(body: &Value) -> Vec<String> {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| match i["id"].as_str() {
            Some(id) => id.to_string(),
            None => {
                let text = i["content"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| i["content"][0]["text"].as_str().unwrap().to_string());
                format!("{}:{text}", i["role"].as_str().unwrap())
            }
        })
        .collect()
}

/// A plain reply answered as `[reasoning, message, reasoning, message]` (a
/// GPT-5.3-codex commentary) keeps each seal where its item stood in the
/// reply's text, and replays in the order answered, not both items ahead of
/// one text (WP8 review W-N4, draft call 5).
#[tokio::test]
async fn a_reply_answered_between_messages_replays_in_that_order() {
    let (mock, state, gw, up) = platform(|req: &Request| {
        answer(
            req,
            &[
                rw::reasoning("rs_1", "First."),
                rw::message("msg_1", "Hm. "),
                rw::reasoning("rs_2", "Second."),
                rw::message("msg_2", "Done."),
            ],
        )
    })
    .await;
    let tid = thread(&gw, json!({"model_alias": "my-model"})).await;
    send(&gw, tid, "hi").await;
    let reply = store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(reply.content, "Hm. Done.");
    assert_eq!(
        reply.reasoning_seals,
        [
            store::ReplySeal::new(0, up.seal("rs_1", SEALED)),
            store::ReplySeal::new(4, up.seal("rs_2", SEALED)),
        ]
    );
    send(&gw, tid, "again").await;
    let sent = posted(&mock).await;
    assert_eq!(
        shape(&sent[1]),
        [
            "user:hi",
            "rs_1",
            "assistant:Hm. ",
            "rs_2",
            "assistant:Done.",
            "user:again"
        ],
        "{}",
        sent[1]
    );
}

/// A partial reply keeps its seal, but no answer of its turn follows the
/// item, which OpenAI refuses ("… provided without its required following
/// item"): the next turn's body leaves the item out, and the reasoning stays
/// in the thread (WP8 review S-R1). A stream cut after the reasoning, an
/// answer whose `max_output_tokens` went to reasoning alone, and the row a
/// Stop after the reasoning leaves.
#[tokio::test]
async fn a_partial_reply_s_seal_is_left_out_where_no_answer_follows_it() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let reasoned = rw::reasoning("rs_1", "Weighing it.");
    let opening = |item: &Value| {
        vec![
            json!({"type": "response.created", "response": {"id": "resp_1",
                   "object": "response", "status": "in_progress", "output": []}}),
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"type": "reasoning", "id": "rs_1"}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                   "summary_index": 0, "item_id": "rs_1", "delta": "Weighing it."}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        ]
    };
    let cut = rw::sse(&opening(&reasoned));
    let mut spent = opening(&reasoned);
    let mut incomplete = rw::response("resp_1", "incomplete", json!([reasoned.clone()]));
    incomplete["incomplete_details"] = json!({"reason": "max_output_tokens"});
    spent.push(json!({"type": "response.incomplete", "response": incomplete}));
    let spent = rw::sse(&spent);

    for (case, first) in [("cut", cut), ("max_output_tokens", spent)] {
        let n = Arc::new(AtomicUsize::new(0));
        let (mock, state, gw, _up) =
            platform(
                move |req: &Request| match n.fetch_add(1, Ordering::Relaxed) {
                    0 => wiremock::ResponseTemplate::new(200)
                        .set_body_raw(first.clone(), "text/event-stream"),
                    _ => answer(req, &[rw::message("msg_2", "Fine.")]),
                },
            )
            .await;
        let tid = thread(&gw, json!({"model_alias": "my-model"})).await;
        send(&gw, tid, "hi").await;
        let reply = store::list_chat_messages(&state.db, tid)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            (reply.content.as_str(), reply.reasoning.as_str()),
            ("", "Weighing it."),
            "{case}"
        );
        assert_eq!(reply.reasoning_seals.len(), 1, "{case}: the seal is kept");
        send(&gw, tid, "again").await;
        let sent = posted(&mock).await;
        assert_eq!(
            shape(&sent[1]),
            ["user:hi", "user:again"],
            "{case}: {}",
            sent[1]
        );
    }

    // A Stop after the reasoning saves the reasoning and its seal, no text.
    let (mock, state, gw, up) =
        platform(|req: &Request| answer(req, &[rw::message("msg_2", "Fine.")])).await;
    let tid = thread(&gw, json!({"model_alias": "my-model"})).await;
    store::append_chat_message(&state.db, tid, "user", "hi", "", None, None, None)
        .await
        .unwrap();
    let stopped = store::ChatReply {
        reasoning: "Weighing it.".into(),
        reasoning_seals: vec![store::ReplySeal::new(0, up.seal("rs_1", SEALED))],
        ..Default::default()
    };
    store::append_chat_reply(&state.db, tid, &stopped)
        .await
        .unwrap();
    send(&gw, tid, "again").await;
    let sent = posted(&mock).await;
    assert_eq!(shape(&sent[0]), ["user:hi", "user:again"], "{}", sent[0]);
}

/// A seal is tied to the row's configuration (WP8 review W-R2): once the
/// owner gives the row another key (another project, perhaps), the old
/// thread's seals are not sent, and the turn replays as on a row that never
/// sealed them; the next answer's seals are the new key's, and go back.
#[tokio::test]
async fn a_seal_goes_back_only_while_its_row_keeps_its_key() {
    let (mock, state, gw, up) = platform(|req: &Request| {
        answer(
            req,
            &[
                rw::reasoning("rs_1", "Weighing it."),
                rw::message("msg_1", "Hello"),
            ],
        )
    })
    .await;
    let tid = thread(&gw, json!({"model_alias": "my-model"})).await;
    send(&gw, tid, "hi").await;
    sqlx::query("UPDATE upstreams SET api_key = 'sk-another-project' WHERE id = ?1")
        .bind(up.id)
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let now = super::Up::of(&state, up.id);
    assert_ne!(now.scope, up.scope);
    send(&gw, tid, "again").await;
    send(&gw, tid, "and again").await;
    let sent = posted(&mock).await;
    assert!(
        reasoning_items(&sent[1]).is_empty(),
        "the old key's seal stays home: {}",
        sent[1]
    );
    assert_eq!(
        shape(&sent[1])[..3],
        ["user:hi", "assistant:Hello", "user:again"]
    );
    // The reply under the new key carries its seal back.
    let back = reasoning_items(&sent[2]);
    assert_eq!(back.len(), 1, "{}", sent[2]);
    let replies: Vec<_> = store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.role == "assistant")
        .collect();
    assert_eq!(replies[0].reasoning_seals[0].seal.config, up.scope.config);
    assert_eq!(replies[1].reasoning_seals[0].seal.config, now.scope.config);
}
