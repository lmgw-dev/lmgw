//! What ends a folder's current thread (client-apps design §3.3): a delete,
//! a move out of the folder, an archive by hand, the folder no longer
//! ongoing — each in its own write's transaction, each a `folder.current`
//! with no thread in the feed, and the next `current` starts a new one.

use serde_json::{json, Value};

use super::{current_ok, folder_of, message, ongoing_folder};
use crate::chat_feed::Feed;
use crate::device_chat::post;
use crate::realtime_chat_thread::world;

/// The `folder.current` events about `folder` read so far.
fn currents(feed: &Feed, folder: i64) -> Vec<Value> {
    feed.named("folder.current")
        .into_iter()
        .filter(|f| f.data["folder_id"] == folder)
        .map(|f| f.data.clone())
        .collect()
}

#[tokio::test]
async fn a_delete_a_move_out_and_an_archive_each_end_the_current_thread_with_a_record() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (_, v) = post(
        &w,
        &owner,
        "/chat/api/folders",
        json!({ "name": "Elsewhere" }),
    )
    .await;
    let elsewhere = v["id"].as_i64().unwrap();
    let mut feed = Feed::open(&w, &owner, "", None).await;

    let actions: [(&str, Value); 3] = [
        ("delete", json!({})),
        ("move", json!({ "folder_id": elsewhere })),
        ("archive", json!({ "archived": true })),
    ];
    let mut expected = Vec::new();
    for (action, body) in &actions {
        let (id, rolled, reason) = current_ok(&w, &owner, folder, false).await;
        assert!(rolled, "{action}");
        assert_eq!(
            reason,
            json!("first"),
            "nothing was current before {action}"
        );
        expected.push(json!({
            "folder_id": folder, "thread_id": id, "previous_thread_id": null,
            "reason": "first", "by": "the dashboard",
        }));
        message(&w, id, 1).await;
        let (s, v) = post(
            &w,
            &owner,
            &format!("/chat/api/threads/{id}/{action}"),
            body.clone(),
        )
        .await;
        assert_eq!(s, 200, "{action}: {v}");
        assert_eq!(
            folder_of(&w, &owner, folder).await.unwrap()["ongoing"]["current_thread_id"],
            json!(null),
            "{action}"
        );
        expected.push(json!({
            "folder_id": folder, "thread_id": null, "previous_thread_id": id,
            "reason": "gone", "by": "the dashboard",
        }));
    }
    feed.until(10, |f| {
        f.iter()
            .filter(|f| f.event == "folder.current" && f.data["folder_id"] == folder)
            .count()
            >= 6
    })
    .await;
    assert_eq!(currents(&feed, folder), expected);
}

#[tokio::test]
async fn a_move_within_the_folder_or_a_pin_keeps_it_current() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (id, _, _) = current_ok(&w, &owner, folder, false).await;
    for (action, body) in [
        ("move", json!({ "folder_id": folder })),
        ("pin", json!({ "pinned": true })),
    ] {
        let (s, v) = post(
            &w,
            &owner,
            &format!("/chat/api/threads/{id}/{action}"),
            body,
        )
        .await;
        assert_eq!(s, 200, "{action}: {v}");
    }
    let (again, rolled, _) = current_ok(&w, &owner, folder, false).await;
    assert_eq!((again, rolled), (id, false));
}

#[tokio::test]
async fn a_folder_that_stops_being_ongoing_loses_its_current_thread() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (id, _, _) = current_ok(&w, &owner, folder, false).await;
    let mut feed = Feed::open(&w, &owner, "", None).await;

    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "ongoing": null }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["ongoing"], json!(null));
    feed.until(10, |f| f.iter().any(|f| f.event == "folder.current"))
        .await;
    assert_eq!(
        currents(&feed, folder),
        vec![json!({
            "folder_id": folder, "thread_id": null, "previous_thread_id": id,
            "reason": "not_ongoing", "by": "the dashboard",
        })]
    );
    // The thread stays, in the folder; marked again, the folder starts anew.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "ongoing": { "idle_minutes": 5 } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["ongoing"],
        json!({ "idle_minutes": 5, "current_thread_id": null })
    );
    let (next, _, reason) = current_ok(&w, &owner, folder, false).await;
    assert_ne!(next, id);
    assert_eq!(reason, json!("first"));
}
