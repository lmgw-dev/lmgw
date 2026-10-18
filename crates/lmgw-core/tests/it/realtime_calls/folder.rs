//! `chat_folder` on a call (§2.2, C23): the POST decides without writing —
//! no thread is made — and the open decides again and binds, a rollover
//! made then; `new=true`; under `ask`, `take_over` binds the folder's
//! current thread as it is at that moment.

use serde_json::json;

use super::{calls_world, Phone};
use crate::chat_ongoing::{current_ok, folder_of, message, ongoing_folder};
use crate::device_chat::pair;
use crate::realtime_chat_thread::World;
use crate::support::realtime_fakes::next_event;

/// The threads in `folder`.
async fn threads_in(w: &World, folder: i64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM chat_threads WHERE folder_id = ?1")
        .bind(folder)
        .fetch_one(&w.state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_post_makes_no_thread_and_the_open_binds_the_one_it_makes() {
    let w = calls_world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let mut p = Phone::call(&w, &owner, &format!("chat_folder={folder}")).await;
    assert_eq!(threads_in(&w, folder).await, 0, "the POST only decides");
    p.c.connect().await;
    let created = p.next_but_states().await;
    assert_eq!(created["type"], "session.created", "{created}");
    let tid = created["session"]["lmgw"]["resolved"]["chat_thread"]["id"]
        .as_i64()
        .unwrap();
    assert_eq!(threads_in(&w, folder).await, 1);
    let current = folder_of(&w, &owner, folder).await.unwrap();
    assert_eq!(current["ongoing"]["current_thread_id"], tid);
    assert_eq!(w.state.calls.get(&p.id).unwrap().thread, Some(tid));
    assert_eq!(p.hangup(&w, &owner).await, 200);

    // With a message in it, new=true starts another at the open.
    message(&w, tid, 1).await;
    let mut q = Phone::connected(&w, &owner, &format!("chat_folder={folder}&new=true")).await;
    let created = q.next_but_states().await;
    let next = created["session"]["lmgw"]["resolved"]["chat_thread"]["id"]
        .as_i64()
        .unwrap();
    assert_ne!(next, tid);
    assert_eq!(threads_in(&w, folder).await, 2);
}

#[tokio::test]
async fn take_over_binds_the_folders_thread_as_it_is_then() {
    let w = calls_world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (first, _, _) = current_ok(&w, &owner, folder, false).await;
    message(&w, first, 1).await;
    // A device holds the current thread.
    let d = pair(&w, "desktop", json!({})).await;
    let mut desktop = w
        .connect_as(&d, &format!("chat_thread={first}"))
        .await
        .unwrap_or_else(|(s, b)| panic!("{s}: {b}"));
    assert_eq!(next_event(&mut desktop).await["type"], "session.created");
    let mut p = Phone::connected(&w, &owner, &format!("chat_folder={folder}&takeover=ask")).await;
    let pending = p.next_but_states().await;
    assert_eq!(pending["held_by"], "device 'desktop'", "{pending}");
    // The folder moves on meanwhile.
    let (second, rolled, _) = current_ok(&w, &owner, folder, true).await;
    assert!(rolled && second != first);
    p.send(json!({"type": "lmgw.session.take_over"}));
    let created = p.next_but_states().await;
    assert_eq!(created["type"], "session.created", "{created}");
    assert_eq!(
        created["session"]["lmgw"]["resolved"]["chat_thread"]["id"],
        second
    );
    // The old thread's holder keeps it.
    crate::support::realtime_fakes::send(
        &mut desktop,
        json!({"type": "session.update", "session": {"type": "realtime"}}),
    )
    .await;
    let ev = crate::realtime_chat_thread::next(&mut desktop).await;
    assert_eq!(ev["type"], "session.updated", "{ev}");
}
