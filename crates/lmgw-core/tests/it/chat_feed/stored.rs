//! Stored events: thread and folder writes, in commit order, resumable by
//! cursor, rendered at delivery; paged catch-up across a restart; `resync`;
//! the sweep's events and the retention prune.

use lmgw_core::state::AppState;
use serde_json::{json, Value};

use super::{set, Feed};
use crate::common::serve;
use crate::device_chat::post;
use crate::realtime_chat_thread::{world, World};

/// As the owner: `POST path body`, which must answer 200; its JSON.
async fn owner(w: &World, path: &str, body: Value) -> Value {
    let (status, v) = post(w, &w.gw.client(), path, body).await;
    assert_eq!(status, 200, "{path}: {v}");
    v
}

#[tokio::test]
async fn writes_arrive_in_commit_order_and_resume_by_since_and_last_event_id() {
    let w = world(|_| {}).await;
    let mut live = Feed::open(&w, &w.gw.client(), "", None).await;
    let start = live.cursor();
    assert_eq!(live.hello()["epoch"], start.split(':').next().unwrap());

    let folder = owner(&w, "/chat/api/folders", json!({ "name": "Kitchen" })).await["id"]
        .as_i64()
        .unwrap();
    let t = owner(
        &w,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "folder_id": folder }),
    )
    .await["id"]
        .as_i64()
        .unwrap();
    owner(
        &w,
        &format!("/chat/api/threads/{t}/settings"),
        json!({ "system_prompt": "be brief" }),
    )
    .await;
    owner(
        &w,
        &format!("/chat/api/threads/{t}/pin"),
        json!({ "pinned": true }),
    )
    .await;
    owner(
        &w,
        &format!("/chat/api/threads/{t}/move"),
        json!({ "folder_id": null }),
    )
    .await;
    owner(
        &w,
        &format!("/chat/api/folders/{folder}"),
        json!({ "name": "Pantry" }),
    )
    .await;
    owner(&w, &format!("/chat/api/threads/{t}/delete"), json!({})).await;
    owner(
        &w,
        &format!("/chat/api/folders/{folder}/delete"),
        json!({ "threads": "keep" }),
    )
    .await;

    let want = vec![
        ("folder.created".to_string(), folder),
        ("thread.created".to_string(), t),
        ("thread.updated".to_string(), t),
        ("thread.updated".to_string(), t),
        ("thread.updated".to_string(), t),
        ("folder.updated".to_string(), folder),
        ("thread.deleted".to_string(), t),
        ("folder.deleted".to_string(), folder),
    ];
    live.until(10, |f| {
        f.iter().filter(|f| f.id.is_some()).count() >= want.len()
    })
    .await;
    assert_eq!(live.stored(), want);
    // Ids are cursors of this database, increasing in commit order, and
    // every change names who made it.
    let seqs: Vec<i64> = live
        .frames
        .iter()
        .filter_map(|f| f.id.as_deref())
        .map(|id| {
            // "<epoch>:<seq>:<tag>" (reviews W5-4, W6-3: 16 random hex digits).
            let mut parts = id.split(':');
            assert_eq!(parts.next().unwrap(), live.hello()["epoch"]);
            let seq = parts.next().unwrap().parse().unwrap();
            assert_eq!(parts.next().map(str::len), Some(16), "{id}");
            seq
        })
        .collect();
    assert!(seqs.windows(2).all(|p| p[0] < p[1]), "{seqs:?}");
    for f in live.frames.iter().filter(|f| f.id.is_some()) {
        assert_eq!(f.data["by"], "the dashboard", "{f:?}");
    }

    // Caught up from the start, each record is rendered as things are now:
    // the thread and the folder are gone, so their creations are tombstones.
    let mut caught = Feed::open(&w, &w.gw.client(), &format!("?since={start}"), None).await;
    caught
        .until(10, |f| {
            f.iter().filter(|f| f.id.is_some()).count() >= want.len()
        })
        .await;
    assert_eq!(caught.stored(), want);
    assert!(caught.named("resync").is_empty());
    let created = &caught.named("thread.created")[0].data;
    assert_eq!(
        (created["thread_id"].clone(), created["deleted"].clone()),
        (json!(t), json!(true)),
        "{created}"
    );
    let folder_created = &caught.named("folder.created")[0].data;
    assert_eq!(folder_created["deleted"], true, "{folder_created}");

    // Last-Event-ID wins over `since` (a browser reconnects with the URL
    // it opened): only what came after the third event.
    let third = live
        .frames
        .iter()
        .filter_map(|f| f.id.clone())
        .nth(2)
        .unwrap();
    let mut resumed =
        Feed::open(&w, &w.gw.client(), &format!("?since={start}"), Some(&third)).await;
    resumed
        .until(10, |f| {
            f.iter().filter(|f| f.id.is_some()).count() >= want.len() - 3
        })
        .await;
    assert_eq!(resumed.hello()["cursor"], third);
    assert_eq!(resumed.stored(), want[3..].to_vec());

    // A cursor that is no cursor is refused, by name.
    let resp =
        w.gw.client()
            .get(format!("{}/chat/api/feed?since=yesterday", w.gw))
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert!(
        v["message"].as_str().unwrap().contains("not a feed cursor"),
        "{v}"
    );
}

#[tokio::test]
async fn catch_up_comes_in_pages_and_across_a_restart() {
    let w = world(|s| s.chat_feed_page_size = 2).await;
    let before = Feed::open(&w, &w.gw.client(), "", None).await.cursor();
    let mut ids = Vec::new();
    for _ in 0..5 {
        let v = owner(&w, "/chat/api/threads", json!({ "model_alias": "chatty" })).await;
        ids.push(v["id"].as_i64().unwrap());
    }

    // A restart: a new process's state on the database the last one wrote.
    // The epoch is the database's, so the cursor still holds.
    let again = AppState::init_for_tests_on(w.state.db.clone())
        .await
        .unwrap();
    let gw = serve(again.clone()).await;
    let client = gw.client();
    let restarted = World {
        state: again,
        gw,
        chat: w.chat,
        tts: w.tts,
        asr: w.asr,
    };
    let mut feed = Feed::open(&restarted, &client, &format!("?since={before}"), None).await;
    feed.until(10, |f| f.iter().filter(|f| f.id.is_some()).count() >= 5)
        .await;
    assert!(feed.named("resync").is_empty(), "{:?}", feed.frames);
    let created: Vec<i64> = feed
        .named("thread.created")
        .iter()
        .map(|f| f.data["id"].as_i64().unwrap())
        .collect();
    assert_eq!(created, ids, "every record, in order, two to a page");
    assert_eq!(
        feed.hello()["epoch"].as_str().unwrap(),
        before.split(':').next().unwrap()
    );

    // Another database's cursor cannot be honoured: resync, then now.
    let other = world(|_| {}).await;
    let mut foreign = Feed::open(
        &other,
        &other.gw.client(),
        &format!("?since={before}"),
        None,
    )
    .await;
    foreign.until(10, |f| f.len() >= 2).await;
    assert_eq!(foreign.frames[1].event, "resync");
    assert!(
        foreign.frames[1].data["reason"]
            .as_str()
            .unwrap()
            .contains("another database"),
        "{:?}",
        foreign.frames[1]
    );
}

#[tokio::test]
async fn the_sweep_is_in_the_feed_and_retention_turns_an_old_cursor_into_a_resync() {
    let w = world(|_| {}).await;
    let mut feed = Feed::open(&w, &w.gw.client(), "", None).await;
    let start = feed.cursor();
    let t = owner(&w, "/chat/api/threads", json!({ "model_alias": "chatty" })).await["id"]
        .as_i64()
        .unwrap();
    sqlx::query("UPDATE chat_threads SET updated_at = datetime('now', '-30 days') WHERE id = ?1")
        .bind(t)
        .execute(&w.state.db)
        .await
        .unwrap();

    // The hourly tick's Chat step: the sweep archives the idle thread, as
    // the gateway's own change.
    lmgw_core::server::chat_upkeep(&w.state).await;
    feed.until(10, |f| f.iter().filter(|f| f.id.is_some()).count() >= 2)
        .await;
    let archived = feed.named("thread.updated")[0];
    assert_eq!(archived.data["id"], t);
    assert!(archived.data["archived_at"].is_string(), "{:?}", archived);
    assert_eq!(
        archived.data["by"],
        Value::Null,
        "the sweep is the gateway's own"
    );

    // Retention 0 keeps every record, however old.
    set(&w, |s| s.chat_feed_retention_days = 0).await;
    sqlx::query("UPDATE chat_feed SET at = datetime('now', '-30 days')")
        .execute(&w.state.db)
        .await
        .unwrap();
    lmgw_core::server::chat_upkeep(&w.state).await;
    let mut kept = Feed::open(&w, &w.gw.client(), &format!("?since={start}"), None).await;
    kept.until(10, |f| f.iter().filter(|f| f.id.is_some()).count() >= 2)
        .await;
    assert!(kept.named("resync").is_empty());

    // At 7 days they go, and a cursor from before them is answered with a
    // resync that names the setting — then the stream goes on from now.
    set(&w, |s| s.chat_feed_retention_days = 7).await;
    lmgw_core::server::chat_upkeep(&w.state).await;
    let mut stale = Feed::open(&w, &w.gw.client(), &format!("?since={start}"), None).await;
    stale.until(10, |f| f.len() >= 2).await;
    assert_eq!(stale.frames[1].event, "resync");
    let reason = stale.frames[1].data["reason"].as_str().unwrap();
    assert!(
        reason.contains("Settings → Chat → Change feed → Keep changes for: 7 days"),
        "{reason}"
    );
    // From now: the newest number (its record pruned, so no check).
    let number = |c: &str| c.split(':').take(2).collect::<Vec<_>>().join(":");
    assert_eq!(
        number(stale.hello()["cursor"].as_str().unwrap()),
        number(&feed.cursor()),
        "from now"
    );
    owner(&w, &format!("/chat/api/threads/{t}/delete"), json!({})).await;
    stale.until(10, |f| has_event(f, "thread.deleted")).await;

    // A cursor newer than the newest event: this database was restored
    // from an older copy.
    let epoch = feed.hello()["epoch"].as_str().unwrap().to_string();
    let mut ahead = Feed::open(&w, &w.gw.client(), &format!("?since={epoch}:99999"), None).await;
    ahead.until(10, |f| f.len() >= 2).await;
    assert_eq!(ahead.frames[1].event, "resync");
    assert!(ahead.frames[1].data["reason"]
        .as_str()
        .unwrap()
        .contains("newer than the newest"));
}

fn has_event(frames: &[super::Frame], event: &str) -> bool {
    frames.iter().any(|f| f.event == event)
}

/// Review W5-4: a copy of the data restored from before a client's cursor
/// (the database and everything beside it) keeps its epoch, and once it has
/// written past that cursor its numbers say nothing is wrong. The cursor's
/// check does: the client resumes with a `resync`, not with events another
/// history numbered the same.
#[tokio::test]
async fn a_restored_copy_that_wrote_past_a_cursor_is_another_database() {
    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let mut feed = Feed::open(&w, &w.gw.client(), "", None).await;
    for _ in 0..3 {
        owner(&w, "/chat/api/threads", json!({ "model_alias": "chatty" })).await;
    }
    feed.until(10, |f| f.iter().filter(|f| f.id.is_some()).count() >= 3)
        .await;
    let cursor = feed.cursor();
    assert_eq!(cursor.split(':').count(), 3, "a tagged cursor: {cursor}");
    let seq: i64 = cursor.split(':').nth(1).unwrap().parse().unwrap();

    // The restore: the copy from before the last two changes.
    sqlx::query("DELETE FROM chat_feed WHERE seq > ?1")
        .bind(seq - 2)
        .execute(&w.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE sqlite_sequence SET seq = ?1 WHERE name = 'chat_feed'")
        .bind(seq - 2)
        .execute(&w.state.db)
        .await
        .unwrap();
    // It writes on, past the cursor, in a later second.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    for _ in 0..3 {
        owner(&w, "/chat/api/folders", json!({ "name": "After" })).await;
    }

    let mut back = Feed::open(&w, &w.gw.client(), "", Some(&cursor)).await;
    back.until(10, |f| f.len() >= 2).await;
    assert_eq!(back.frames[1].event, "resync", "{:#?}", back.frames);
    assert!(back.frames[1].data["reason"]
        .as_str()
        .unwrap()
        .contains("restored from an older copy"));
    // A cursor this history handed out resumes without one.
    let fresh = back.hello()["cursor"].as_str().unwrap().to_string();
    let again = Feed::open(&w, &w.gw.client(), "", Some(&fresh)).await;
    assert!(again.named("resync").is_empty(), "{:#?}", again.frames);
}
