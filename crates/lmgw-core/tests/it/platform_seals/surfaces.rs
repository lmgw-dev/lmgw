//! Seals are the upstream's to read, never a page's, a device's or an
//! export's (WP8 review S-R2): a tool turn's record keeps its seals for the
//! next turn's replay, and `GET /chat/api/threads/{id}` and both exports
//! show that record without them, its reasoning text kept. The chat feed
//! renders the same messages (`tests/it/chat_feed/messages.rs`).

use lmgw_core::store;
use serde_json::{json, Value};

use super::{sealed_record, SEALED};
use crate::chat_golden::thread;

#[tokio::test]
async fn the_thread_and_both_exports_show_a_record_without_its_seals() {
    let (_mock, state, gw, up) = super::platform(|req| {
        super::answer(
            req,
            &[crate::support::responses_wire::message("msg_1", "Fine.")],
        )
    })
    .await;
    let tid = thread(&gw, json!({"model_alias": "my-model"})).await;
    store::append_chat_message(&state.db, tid, "user", "hi", "", None, None, None)
        .await
        .unwrap();
    store::append_chat_reply(
        &state.db,
        tid,
        &store::ChatReply {
            content: "Done.".into(),
            ir_messages: Some(sealed_record(&up)),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let get = |route: String| {
        let client = gw.client();
        let url = format!("{gw}{route}");
        async move { client.get(url).send().await.unwrap().text().await.unwrap() }
    };
    let listed = get(format!("/chat/api/threads/{tid}")).await;
    let v: Value = serde_json::from_str(&listed).unwrap();
    let record = v["messages"][1]["ir_messages"].as_str().unwrap();
    assert!(record.contains("Use the tool."), "{record}");
    for (what, body) in [
        ("the thread", listed.clone()),
        (
            "the JSON export",
            get(format!("/chat/api/threads/{tid}/export?format=json")).await,
        ),
        (
            "the Markdown export",
            get(format!("/chat/api/threads/{tid}/export?format=md")).await,
        ),
    ] {
        assert!(body.contains("Use the tool."), "{what}: {body}");
        assert!(
            !body.contains(SEALED) && !body.contains("\"sealed\""),
            "{what}: {body}"
        );
    }
    // The store keeps them for the replay.
    let row = store::list_chat_messages(&state.db, tid)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(row.ir_messages.unwrap().contains(SEALED));
}

/// The Traffic · Responses view of a stored loop answer shows its reasoning
/// item's encrypted content as its size only (WP8 verify V-F3): in a loop
/// answer it is the whole carrier, and `/api` readers include admin
/// devices. The store keeps it for the chain's next turn.
#[tokio::test]
async fn the_responses_view_shows_a_stored_seal_as_its_size() {
    use crate::support::responses_wire as rw;
    let (_mock, state, gw, _up) = super::platform(|req| {
        super::answer(
            req,
            &[
                rw::reasoning("rs_1", "Weighing it."),
                rw::message("msg_1", "Hello"),
            ],
        )
    })
    .await;
    let v: Value = crate::chat_actions::post(
        &gw,
        "/v1/responses",
        json!({"model": "my-model", "input": "hi", "store": true,
               "tools": [{"type": "mcp", "server_label": "docs"}]}),
    )
    .await
    .json()
    .await
    .unwrap();
    let id = v["id"].as_str().unwrap();
    let stored = store::get_response(&state.db, id, store::ResponseReader::Owner)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.body.contains(SEALED), "the store keeps it");
    let chain = crate::models_full_wire::read(&gw, &format!("/api/responses/chain?id={id}")).await;
    let shown = chain["responses"][0]["body"].as_str().unwrap();
    assert!(!shown.contains(SEALED), "{shown}");
    assert!(shown.contains("<sealed reasoning, "), "{shown}");
    assert!(shown.contains("Weighing it."), "the summary stays: {shown}");
}
