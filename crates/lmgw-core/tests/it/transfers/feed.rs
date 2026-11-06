//! `transfer.updated` on the Chat feed (device transfers design §6): a
//! stored event to the sender, to the receiver from `ready` on, and to the
//! owner; to no other device; replayed to a device that was away.

use serde_json::Value;

use super::{call, device, file, offer, put, two, APP};
use crate::chat_feed::Feed;

/// The `transfer.updated` frames of `feed`: `(state, id)`.
fn updates(feed: &Feed) -> Vec<(String, String)> {
    feed.named("transfer.updated")
        .into_iter()
        .map(|f| {
            assert!(f.id.is_some(), "a stored event: {f:?}");
            (
                f.data["state"].as_str().unwrap().to_string(),
                f.data["id"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// Wait until `feed` holds the `thread.created` of thread `id`: every
/// record before it was rendered for this reader.
async fn past(feed: &mut Feed, id: i64) {
    feed.until(|f| {
        f.iter()
            .any(|f| f.event == "thread.created" && f.data["id"] == id)
    })
    .await;
}

#[tokio::test]
async fn the_parties_and_the_owner_hear_it_and_no_one_else() {
    let (w, desktop, phone) = two().await;
    let tablet = device(&w, "tablet", Some(APP)).await;
    let owner = w.gw.client();
    let mut sender = Feed::open(&w, &desktop.client, "", None).await;
    let mut receiver = Feed::open(&w, &phone.client, "", None).await;
    let mut third = Feed::open(&w, &tablet.client, "", None).await;
    let mut dashboard = Feed::open(&w, &owner, "", None).await;

    let bytes = file(2000);
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    // Canceled during its upload: the receiver never hears of this one.
    let early = offer(&w, &desktop, &phone, &bytes).await;
    let early = early["id"].as_str().unwrap().to_string();
    call(
        &w,
        &desktop.client,
        "POST",
        &format!("/transfers/api/transfers/{early}/cancel"),
        None,
    )
    .await;
    let (s, v) = put(&w, &desktop.client, &id, 0, &bytes, 2000).await;
    assert_eq!(s, 200, "{v}");
    let (s, _) = call(
        &w,
        &phone.client,
        "POST",
        &format!("/transfers/api/transfers/{id}/received"),
        None,
    )
    .await;
    assert_eq!(s, 200);
    // A thread every reader sees, after them all.
    let marker = crate::device_chat::chat_thread(&w, &owner, "chatty").await;
    for feed in [&mut sender, &mut receiver, &mut third, &mut dashboard] {
        past(feed, marker).await;
    }
    // Rendered at delivery: as the transfer is then.
    assert_eq!(updates(&sender).len(), 5, "{:#?}", sender.frames);
    assert_eq!(updates(&dashboard).len(), 5);
    let heard = updates(&receiver);
    assert_eq!(heard.len(), 2, "ready and delivered: {heard:?}");
    assert!(heard.iter().all(|(_, i)| *i == id), "never the early one");
    assert!(updates(&third).is_empty(), "{:#?}", third.frames);
    let last: &Value = &receiver.named("transfer.updated").last().unwrap().data;
    assert_eq!(last["state"], "delivered");
}

/// A device that was away gets the events of its transfers in its
/// catch-up, from its cursor.
#[tokio::test]
async fn a_feed_reopened_from_its_cursor_replays_them() {
    let (w, desktop, phone) = two().await;
    let away = Feed::open(&w, &phone.client, "", None).await;
    let cursor = away.cursor();
    drop(away);
    let bytes = file(500);
    let t = offer(&w, &desktop, &phone, &bytes).await;
    let id = t["id"].as_str().unwrap().to_string();
    put(&w, &desktop.client, &id, 0, &bytes, 500).await;
    let mut back = Feed::open(&w, &phone.client, "", Some(&cursor)).await;
    back.until(|f| f.iter().any(|f| f.event == "transfer.updated"))
        .await;
    assert_eq!(updates(&back), [("ready".to_string(), id.clone())]);
    // And the list a receiver reads at its start says the same.
    let (_, l) = call(
        &w,
        &phone.client,
        "GET",
        "/transfers/api/transfers?role=incoming",
        None,
    )
    .await;
    assert_eq!(l["transfers"][0]["id"], id.as_str());
}
