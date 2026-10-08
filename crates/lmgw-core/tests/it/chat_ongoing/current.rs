//! `POST /chat/api/folders/{id}/current` (client-apps design §3.2, §3.3, L8).

use serde_json::json;

use super::{current, current_ok, folder_of, message, ongoing_folder};
use crate::device_chat::{get, pair, post};
use crate::realtime_chat_thread::world;

#[tokio::test]
async fn the_first_call_starts_the_conversation_and_the_next_ones_answer_it() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({ "temperature": 0.3 })).await;
    assert_eq!(
        folder_of(&w, &owner, folder).await.unwrap()["ongoing"]["current_thread_id"],
        json!(null)
    );

    let (s, v) = current(&w, &owner, folder, false).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (&v["rolled_over"], &v["reason"]),
        (&json!(true), &json!("first"))
    );
    assert!(
        v["note"].as_str().unwrap().contains("no current thread"),
        "{v}"
    );
    let t = &v["thread"];
    let id = t["id"].as_i64().unwrap();
    // A chat thread in the folder, from its defaults, as GET answers it.
    assert_eq!(
        (
            &t["kind"],
            &t["folder_id"],
            &t["model_alias"],
            &t["temperature"]
        ),
        (
            &json!("chat"),
            &json!(folder),
            &json!("chatty"),
            &json!(0.3)
        )
    );
    assert!(
        t.get("continue").is_some() && t.get("voice_resolved").is_some(),
        "{t}"
    );
    assert_eq!(
        folder_of(&w, &owner, folder).await.unwrap()["ongoing"]["current_thread_id"],
        json!(id)
    );

    // Again: the same thread, nothing started.
    message(&w, id, 1).await;
    let (s, v) = current(&w, &owner, folder, false).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["thread"]["id"], id);
    assert_eq!(
        (&v["rolled_over"], &v["reason"], &v["note"]),
        (&json!(false), &json!(null), &json!(null))
    );
}

#[tokio::test]
async fn new_reuses_an_empty_thread_and_starts_one_once_it_has_messages() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 0, json!({})).await;
    let (first, _, _) = current_ok(&w, &owner, folder, false).await;

    // Empty: reused, not rolled over, so no pile of empty threads.
    let (again, rolled, reason) = current_ok(&w, &owner, folder, true).await;
    assert_eq!((again, rolled, reason), (first, false, json!(null)));

    message(&w, first, 1).await;
    let (next, rolled, reason) = current_ok(&w, &owner, folder, true).await;
    assert_ne!(next, first);
    assert_eq!((rolled, reason), (true, json!("requested")));
    let (s, v) = get(&w, &owner, &format!("/chat/api/threads/{first}")).await;
    assert_eq!(s, 200, "the old thread stays: {v}");
    assert_eq!(v["thread"]["folder_id"], folder);
}

#[tokio::test]
async fn idleness_is_measured_from_the_newest_message() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 60, json!({})).await;
    let (id, _, _) = current_ok(&w, &owner, folder, false).await;

    // Empty is never idle, however old the thread.
    sqlx::query("UPDATE chat_threads SET created_at = datetime('now', '-3 days'), updated_at = datetime('now', '-3 days') WHERE id = ?1")
        .bind(id)
        .execute(&w.state.db)
        .await
        .unwrap();
    assert_eq!(current_ok(&w, &owner, folder, false).await.0, id);

    // An old message, but a recent settings change: `updated_at` is not the
    // clock, the newest message is.
    message(&w, id, 120).await;
    let (s, _) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{id}/settings"),
        json!({ "temperature": 0.9 }),
    )
    .await;
    assert_eq!(s, 200);
    let (s, v) = current(&w, &owner, folder, false).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (&v["rolled_over"], &v["reason"]),
        (&json!(true), &json!("idle"))
    );
    let note = v["note"].as_str().unwrap();
    assert!(
        note.contains("60 minute") && note.contains("idle minutes"),
        "{note}"
    );
    let idle_next = v["thread"]["id"].as_i64().unwrap();

    // A newer message keeps it current.
    message(&w, idle_next, 120).await;
    message(&w, idle_next, 5).await;
    assert_eq!(current_ok(&w, &owner, folder, false).await.0, idle_next);

    // 0: only on request.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "ongoing": { "idle_minutes": 0 } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    sqlx::query(
        "UPDATE chat_messages SET created_at = datetime('now', '-100 days') WHERE thread_id = ?1",
    )
    .bind(idle_next)
    .execute(&w.state.db)
    .await
    .unwrap();
    assert_eq!(current_ok(&w, &owner, folder, false).await.0, idle_next);
}

#[tokio::test]
async fn a_folder_that_is_not_ongoing_or_names_no_model_is_refused_by_name() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let (_, v) = post(&w, &owner, "/chat/api/folders", json!({ "name": "Plain" })).await;
    let plain = v["id"].as_i64().unwrap();
    assert_eq!(v["ongoing"], json!(null));
    let (s, v) = current(&w, &owner, plain, false).await;
    assert_eq!((s, &v["code"]), (409, &json!("not_ongoing")), "{v}");
    let (s, v) = current(&w, &owner, 9_999, false).await;
    assert_eq!((s, &v["code"]), (404, &json!("not_found")), "{v}");

    // Ongoing needs a model, on create and on a patch, and a patch cannot
    // take the model away while it stays ongoing.
    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/folders",
        json!({ "name": "A", "ongoing": { "idle_minutes": 10 } }),
    )
    .await;
    assert_eq!(s, 400, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("defaults.model_alias"),
        "{v}"
    );
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{plain}"),
        json!({ "ongoing": { "idle_minutes": 10 } }),
    )
    .await;
    assert_eq!(s, 400, "{v}");
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{plain}"),
        json!({ "ongoing": { "idle_minutes": -1 }, "defaults": { "model_alias": "chatty" } }),
    )
    .await;
    assert_eq!(s, 400, "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("idle_minutes"),
        "{v}"
    );
    let folder = ongoing_folder(&w, &owner, "B", 10, json!({})).await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": {} }),
    )
    .await;
    assert_eq!(s, 400, "{v}");

    // A folder whose model was removed by a hand-edited row: 409.
    sqlx::query("UPDATE chat_folders SET defaults = '{}' WHERE id = ?1")
        .bind(folder)
        .execute(&w.state.db)
        .await
        .unwrap();
    let (s, v) = current(&w, &owner, folder, false).await;
    assert_eq!((s, &v["code"]), (409, &json!("folder_no_model")), "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("set the folder's model"),
        "{v}"
    );
}

#[tokio::test]
async fn two_callers_at_once_get_one_thread() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let d = pair(&w, "phone", json!({})).await;
    let e = pair(&w, "desktop", json!({})).await;
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;

    // Two devices and the dashboard, several times each, all at once —
    // first on a folder with no thread, then asking for a new one.
    for new in [false, true] {
        if new {
            let (id, _, _) = current_ok(&w, &owner, folder, false).await;
            message(&w, id, 1).await;
        }
        let calls = (0..4)
            .flat_map(|_| [&owner, &d.client, &e.client])
            .map(|c| current(&w, c, folder, new));
        let answers = futures::future::join_all(calls).await;
        let mut ids: Vec<i64> = answers
            .iter()
            .map(|(s, v)| {
                assert_eq!(*s, 200, "{v}");
                v["thread"]["id"].as_i64().unwrap()
            })
            .collect();
        let rolled = answers
            .iter()
            .filter(|(_, v)| v["rolled_over"] == true)
            .count();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 1, "one thread for every caller: {ids:?}");
        assert_eq!(rolled, 1, "exactly one call started it");
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_threads WHERE folder_id = ?1")
        .bind(folder)
        .fetch_one(&w.state.db)
        .await
        .unwrap();
    assert_eq!(n, 2, "the first thread and the one asked for");
}

#[tokio::test]
async fn a_chat_thread_created_in_the_folder_becomes_current_and_an_admin_or_moved_one_does_not() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (first, _, _) = current_ok(&w, &owner, folder, false).await;

    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({ "model_alias": "other", "folder_id": folder }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let by_hand = v["id"].as_i64().unwrap();
    assert_eq!(v["model_alias"], "chatty", "the folder's model");
    let (id, rolled, _) = current_ok(&w, &owner, folder, false).await;
    assert_eq!((id, rolled), (by_hand, false));

    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "kind": "admin", "folder_id": folder }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let elsewhere = crate::device_chat::chat_thread(&w, &owner, "chatty").await;
    let (s, _) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{elsewhere}/move"),
        json!({ "folder_id": folder }),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(current_ok(&w, &owner, folder, false).await.0, by_hand);
    assert_ne!(first, by_hand);
}

/// `new` is optional, and so is the body (review W5-10).
#[tokio::test]
async fn current_takes_no_body() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let resp = owner
        .post(format!("{}/chat/api/folders/{folder}/current", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["reason"], "first", "{v}");
}

/// A thread a turn answers now is never idle (review W5-11): another
/// client's `current` keeps it, however old its newest message is.
#[tokio::test]
async fn a_thread_with_a_running_turn_is_not_idle() {
    use std::sync::Arc;

    use crate::support::realtime_fakes::Turn;

    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (tid, _, _) = current_ok(&w, &owner, folder, false).await;
    // A turn that hangs at its upstream, as a long tool loop or a slow
    // model does.
    let release = Arc::new(tokio::sync::Notify::new());
    w.chat
        .push(Turn::Held(release.clone(), Box::new(Turn::text(&["late"]))));
    let send = {
        let client = owner.clone();
        let url = format!("{}/chat/api/threads/{tid}/send", w.gw);
        tokio::spawn(async move {
            client
                .post(url)
                .json(&json!({ "content": "hello" }))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        })
    };
    for _ in 0..100 {
        if w.chat.seen.chat_count() > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(w.chat.seen.chat_count(), 1, "the turn reached its upstream");
    // Its messages written an hour ago: idle by the folder's 30 minutes.
    sqlx::query(
        "UPDATE chat_messages SET created_at = datetime('now', '-60 minutes') WHERE thread_id = ?1",
    )
    .bind(tid)
    .execute(&w.state.db)
    .await
    .unwrap();
    let (again, rolled, _) = current_ok(&w, &owner, folder, false).await;
    assert_eq!((again, rolled), (tid, false), "the running turn keeps it");
    release.notify_one();
    let frames = send.await.unwrap();
    assert!(frames.contains("late"), "{frames}");
}

/// The rollover's write is a compare-and-set on the pointer (review W5-6):
/// one that read a current thread a delete, a move or an archive has ended
/// since writes nothing — no thread, no record — and `current` decides
/// again on what is there now.
#[tokio::test]
async fn the_rollover_writes_only_onto_the_pointer_it_read() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (first, _, _) = current_ok(&w, &owner, folder, false).await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{first}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let t = lmgw_core::store::ChatThread {
        model_alias: "chatty".into(),
        kind: "chat".into(),
        folder_id: Some(folder),
        ..Default::default()
    };
    let head = || async {
        lmgw_core::store::feed::bounds(&w.state.db)
            .await
            .unwrap()
            .head
    };
    let threads = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM chat_threads")
            .fetch_one(&w.state.db)
            .await
            .unwrap()
    };
    let (before, count) = (head().await, threads().await);
    let stale =
        lmgw_core::store::create_current_thread(&w.state.db, folder, &t, Some(first), "idle", None)
            .await
            .unwrap();
    assert_eq!(stale, None, "the pointer moved: nothing written");
    assert_eq!((head().await, threads().await), (before, count));
    let fresh =
        lmgw_core::store::create_current_thread(&w.state.db, folder, &t, None, "first", None)
            .await
            .unwrap();
    assert!(fresh.is_some());
    let (again, rolled, _) = current_ok(&w, &owner, folder, false).await;
    assert_eq!((Some(again), rolled), (fresh, false));
}

/// A thread's `last_message_at`: when its newest message was written, in
/// unix seconds, `null` without one — in `current`'s thread, the thread
/// list and the feed's `thread.*`, so a client times the idle rollover
/// from the gateway's own clock of the conversation.
#[tokio::test]
async fn a_thread_says_when_its_newest_message_came() {
    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let owner = w.gw.client();
    let folder = super::ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (s, v) = super::current(&w, &owner, folder, false).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["thread"]["last_message_at"], json!(null), "{v}");
    let tid = v["thread"]["id"].as_i64().unwrap();
    super::message(&w, tid, 5).await;
    let now = chrono::Utc::now().timestamp();
    let (_, v) = super::current(&w, &owner, folder, false).await;
    let at = v["thread"]["last_message_at"].as_i64().expect("a time");
    assert!(
        (now - 5 * 60 - at).abs() <= 5,
        "five minutes ago: {at} vs {now}"
    );
    let (_, list) = crate::device_chat::get(&w, &owner, "/chat/api/threads").await;
    let row = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == tid)
        .unwrap()
        .clone();
    assert_eq!(row["last_message_at"], json!(at), "{row}");
}
