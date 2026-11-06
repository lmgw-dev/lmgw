//! `POST /chat/api/threads/{id}/title`: the owner's rename, its refusals
//! (empty, temporary, out of a device's reach), the feed event, and that a
//! title set by hand is not replaced by the first send's own naming.

use serde_json::{json, Value};

use crate::chat_feed::{subject, Feed};
use crate::device_chat::{chat_thread, device_world, get, post, Hidden};
use crate::realtime_chat_thread::{world, World};
use crate::support::realtime_fakes::Turn;

fn title_path(id: i64) -> String {
    format!("/chat/api/threads/{id}/title")
}

/// `content` sent into `id` as the owner, the stream read to its end.
async fn send(w: &World, id: i64, content: &str) {
    w.chat.push(Turn::text(&["Fine."]));
    let resp =
        w.gw.client()
            .post(format!("{}/chat/api/threads/{id}/send", w.gw))
            .json(&json!({ "content": content }))
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    resp.text().await.unwrap();
}

async fn title_of(w: &World, id: i64) -> Value {
    let (s, v) = get(w, &w.gw.client(), &format!("/chat/api/threads/{id}")).await;
    assert_eq!(s, 200, "{v}");
    v["thread"]["title"].clone()
}

#[tokio::test]
async fn the_owner_renames_a_thread_and_the_title_is_trimmed() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let id = chat_thread(&w, &owner, "chatty").await;
    let (s, v) = post(
        &w,
        &owner,
        &title_path(id),
        json!({ "title": "  Tax 2026  " }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["id"], id);
    assert_eq!(v["title"], "Tax 2026");
    assert_eq!(title_of(&w, id).await, "Tax 2026");
}

#[tokio::test]
async fn an_empty_title_is_a_400_and_changes_nothing() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let id = chat_thread(&w, &owner, "chatty").await;
    for t in ["", "   \n\t"] {
        let (s, v) = post(&w, &owner, &title_path(id), json!({ "title": t })).await;
        assert_eq!((s, v["code"].as_str()), (400, Some("bad_request")), "{v}");
    }
    assert_eq!(title_of(&w, id).await, "New chat");
}

#[tokio::test]
async fn an_unknown_field_is_refused() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let id = chat_thread(&w, &owner, "chatty").await;
    let (s, _) = post(
        &w,
        &owner,
        &title_path(id),
        json!({ "title": "x", "pin": 1 }),
    )
    .await;
    assert_eq!(s, 422);
}

#[tokio::test]
async fn a_temporary_thread_is_a_409() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "temporary": true }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_i64().unwrap();
    assert!(id < 0);
    let (s, v) = post(&w, &owner, &title_path(id), json!({ "title": "Keep" })).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (409, Some("temporary_thread")),
        "{v}"
    );
}

#[tokio::test]
async fn a_device_renames_what_it_reaches_and_gets_a_404_for_the_rest() {
    let (w, d) = device_world().await;
    let mine = chat_thread(&w, &d.client, "chatty").await;
    let (s, v) = post(&w, &d.client, &title_path(mine), json!({ "title": "Mine" })).await;
    assert_eq!((s, v["title"].as_str()), (200, Some("Mine")), "{v}");

    for hidden in Hidden::BOTH {
        let h = hidden.make(&w).await;
        let (s, v) = post(&w, &d.client, &title_path(h.id), json!({ "title": "Spy" })).await;
        assert_eq!(
            (s, v["code"].as_str()),
            (404, Some("not_found")),
            "{hidden:?}: {v}"
        );
        assert_ne!(title_of(&w, h.id).await, "Spy", "{hidden:?}");
    }
    let (s, v) = post(&w, &d.client, &title_path(987_654), json!({ "title": "x" })).await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");
}

#[tokio::test]
async fn a_rename_reaches_the_feed_as_thread_updated() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let id = chat_thread(&w, &owner, "chatty").await;
    let mut feed = Feed::open(&w, &owner, "", None).await;
    post(&w, &owner, &title_path(id), json!({ "title": "Renamed" })).await;
    feed.until(|f| f.iter().any(|f| f.event == "thread.updated"))
        .await;
    let updated = feed.named("thread.updated");
    assert_eq!(subject(updated.last().unwrap()), id);
}

#[tokio::test]
async fn the_first_send_does_not_replace_a_title_set_by_hand() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let renamed = chat_thread(&w, &owner, "chatty").await;
    let plain = chat_thread(&w, &owner, "chatty").await;
    post(&w, &owner, &title_path(renamed), json!({ "title": "Mine" })).await;

    send(&w, renamed, "what about the roof").await;
    send(&w, plain, "what about the roof").await;

    assert_eq!(title_of(&w, renamed).await, "Mine");
    assert_eq!(
        title_of(&w, plain).await,
        "what about the roof",
        "control: an unrenamed thread is still named by its first send"
    );
}
