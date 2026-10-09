//! The feed's `profile.created`, `profile.updated` and `profile.deleted`
//! events (personality-profiles design §3.2): recorded with each write, in
//! commit order with the threads and folders a delete clears, delivered to
//! a device, and repeated as `profile.created` after a `resync`.

use serde_json::{json, Value};

use super::Feed;
use crate::device_chat::{device_world, post};

#[tokio::test]
async fn each_write_is_an_event_every_reader_hears_and_a_resync_repeats_the_list() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let mut mine = Feed::open(&w, &owner, "", None).await;
    let mut theirs = Feed::open(&w, &d.client, "", None).await;

    let (s, p) = post(&w, &owner, "/chat/api/profiles", json!({"name": "Calm"})).await;
    assert_eq!(s, 200, "{p}");
    let id = p["id"].as_i64().unwrap();
    let (_, t) = post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({"model_alias": "chatty"}),
    )
    .await;
    let tid = t["id"].as_i64().unwrap();
    post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"profile_id": id}),
    )
    .await;
    post(
        &w,
        &d.client,
        &format!("/chat/api/profiles/{id}"),
        json!({"name": "Calmer"}),
    )
    .await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/profiles/{id}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200, "{v}");

    let done = |f: &[super::Frame]| f.iter().any(|f| f.event == "profile.deleted");
    mine.until(10, done).await;
    theirs.until(10, done).await;
    for feed in [&mine, &theirs] {
        let events: Vec<(String, Value)> = feed
            .frames
            .iter()
            .filter(|f| f.event.starts_with("profile.") || f.event.starts_with("thread."))
            .map(|f| {
                assert!(f.id.is_some(), "a stored event: {f:?}");
                (f.event.clone(), f.data.clone())
            })
            .collect();
        let profile = |name: &str, by: &str| json!({"id": id, "name": name, "by": by});
        let kinds: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "profile.created",
                "thread.created",
                "thread.updated",
                "profile.updated",
                // The delete: its thread first, then the profile.
                "thread.updated",
                "profile.deleted",
            ],
            "{events:#?}"
        );
        assert_eq!(events[0].1, profile("Calm", "the dashboard"));
        assert_eq!(events[3].1, profile("Calmer", "device 'phone'"));
        assert_eq!(events[5].1, profile("Calmer", "the dashboard"));
        assert_eq!(events[4].1["profile_id"], Value::Null);
    }

    // A cursor the feed cannot honour: `resync`, then every profile as
    // `profile.created` with no id and no author.
    let mut again = Feed::open(&w, &d.client, "?since=another-db:5", None).await;
    again
        .until(10, |f| f.iter().any(|f| f.event == "profile.created"))
        .await;
    assert_eq!(again.frames[1].event, "resync", "{:?}", again.frames);
    let listed: Vec<&super::Frame> = again.named("profile.created");
    assert_eq!(listed.len(), 1, "only Concise is left: {listed:?}");
    assert_eq!(listed[0].data["name"], "Concise");
    assert_eq!(listed[0].data["by"], Value::Null);
    assert!(listed[0].id.is_none(), "{:?}", listed[0]);
    assert_eq!(again.frames[2].event, "profile.created", "right after it");
}
