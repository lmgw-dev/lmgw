//! L3 and L5 for `current` (client-apps design §3, L3): a device asking for
//! an admin folder's current thread gets the 404 of any folder that is not
//! there; a rollover never hands it a thread that drives the self-admin
//! plane; the thread it starts is a chat thread, its own; and the feed never
//! tells it of a folder or a thread out of its reach through
//! `folder.current` (review W4-6).

use serde_json::{json, Value};

use super::{current, current_ok, folder_of, message, ongoing_folder};
use crate::chat_feed::Feed;
use crate::device_chat::{device_world, get, pair, post, rows_of, sse};

const LMGW: &str = r#"[{"server_label":"lmgw"}]"#;

#[tokio::test]
async fn a_device_gets_the_404_of_any_missing_folder_for_an_admin_one() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let admin = ongoing_folder(
        &w,
        &owner,
        "Ops",
        30,
        json!({ "mcp_tools": serde_json::from_str::<Value>(LMGW).unwrap() }),
    )
    .await;
    // The owner's conversation there runs.
    let (mine, _, _) = current_ok(&w, &owner, admin, false).await;

    let (s, missing) = current(&w, &d.client, 9_999, false).await;
    assert_eq!(s, 404);
    for new in [false, true] {
        let (s, v) = current(&w, &d.client, admin, new).await;
        assert_eq!(
            (s, &v),
            (404, &missing),
            "the same 404 as a folder not there"
        );
    }
    // And nothing moved for the owner.
    let (again, rolled, _) = current_ok(&w, &owner, admin, false).await;
    assert_eq!((again, rolled), (mine, false));
}

#[tokio::test]
async fn a_rollover_never_hands_a_device_a_thread_that_drives_the_self_admin_plane() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (first, _, _) = current_ok(&w, &d.client, folder, false).await;
    message(&w, first, 1).await;
    // The owner attaches the self-admin toolset to the conversation.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{first}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    // The folder no longer names it to the device; the owner still sees it.
    assert_eq!(
        folder_of(&w, &d.client, folder).await.unwrap()["ongoing"]["current_thread_id"],
        json!(null)
    );
    assert_eq!(
        folder_of(&w, &owner, folder).await.unwrap()["ongoing"]["current_thread_id"],
        json!(first)
    );

    // Asked for — plainly, or for a new one — the device gets a new chat
    // thread, never the hidden one.
    for new in [false, true] {
        let (s, v) = current(&w, &d.client, folder, new).await;
        assert_eq!(s, 200, "{v}");
        let t = &v["thread"];
        assert_ne!(t["id"], first);
        assert_eq!(t["kind"], "chat");
        assert!(
            !t["mcp_tools"].to_string().contains("lmgw"),
            "a thread without the toolset: {t}"
        );
    }
    let (s, v) = current(&w, &d.client, folder, false).await;
    assert_eq!(s, 200);
    let (next, _, _) = current_ok(&w, &owner, folder, false).await;
    assert_eq!(
        v["thread"]["id"], next,
        "the conversation moved on for everyone"
    );
    let (s, _) = get(&w, &d.client, &format!("/chat/api/threads/{first}")).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn the_thread_a_device_starts_is_a_chat_thread_of_its_own_within_its_scope() {
    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let owner = w.gw.client();
    let d = pair(
        &w,
        "phone",
        json!({ "scope_mode": "allow", "scope_patterns": "chatty" }),
    )
    .await;
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let mut feed = Feed::open(&w, &d.client, "", None).await;
    let (first, _, _) = current_ok(&w, &d.client, folder, false).await;
    message(&w, first, 1).await;
    let (s, v) = current(&w, &d.client, folder, true).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["reason"], "requested");
    let id = v["thread"]["id"].as_i64().unwrap();
    assert_eq!(v["thread"]["kind"], "chat");

    // Attributed to the device in the feed: the thread and the move.
    feed.until(10, |f| {
        f.iter()
            .filter(|f| f.event == "folder.current" && f.data["folder_id"] == folder)
            .count()
            >= 2
    })
    .await;
    let created = feed
        .named("thread.created")
        .into_iter()
        .find(|f| f.data["id"] == id)
        .unwrap()
        .data
        .clone();
    assert_eq!(created["by"], "device 'phone'");
    let moved = feed.named("folder.current").last().unwrap().data.clone();
    assert_eq!(
        moved,
        json!({ "folder_id": folder, "thread_id": id, "previous_thread_id": first,
                "reason": "requested", "by": "device 'phone'" })
    );

    // Its turns there run as the device (L4).
    let (s, frames) = sse(
        &w,
        &d.client,
        &format!("/chat/api/threads/{id}/send"),
        json!({ "content": "hello" }),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    let rows = rows_of(&w, d.id).await;
    assert!(
        rows.iter()
            .any(|(_, alias, status, _)| alias == "chatty" && *status == 200),
        "the turn's row is the device's: {rows:?}"
    );
}

#[tokio::test]
async fn folder_current_never_tells_a_device_of_a_folder_out_of_its_reach() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let mut live = Feed::open(&w, &d.client, "", None).await;
    let start = live.cursor();
    let (first, _, _) = current_ok(&w, &owner, folder, false).await;
    // The device hears that one while the folder is plain.
    live.until(10, |f| f.iter().any(|f| f.event == "folder.current"))
        .await;

    // The folder's defaults take the toolset, not the thread (the opt-out),
    // and then the thread goes: that record is about a folder the device
    // cannot see.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{folder}"),
        json!({ "defaults": { "model_alias": "chatty", "mcp_tools": [{ "server_label": "lmgw" }] },
                "apply_to_current": false }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, _) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{first}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200);
    // Something the device does see, last.
    let marker = crate::device_chat::chat_thread(&w, &owner, "chatty").await;
    let seen = |f: &[crate::chat_feed::Frame]| {
        f.iter()
            .any(|f| f.event == "thread.created" && f.data["id"] == marker)
    };
    live.until(10, seen).await;
    let currents = |feed: &Feed| -> Vec<Value> {
        feed.named("folder.current")
            .into_iter()
            .map(|f| f.data.clone())
            .collect()
    };
    assert_eq!(currents(&live).len(), 1, "live: {:#?}", live.frames);
    assert_eq!(live.named("folder.deleted")[0].data["folder_id"], folder);

    // Caught up from before it all, now that the folder is out of reach:
    // not even the record written while it was plain.
    let mut late = Feed::open(&w, &d.client, &format!("?since={start}"), None).await;
    late.until(10, seen).await;
    assert!(currents(&late).is_empty(), "catch-up: {:#?}", late.frames);

    // The owner hears both.
    let mut mine = Feed::open(&w, &owner, &format!("?since={start}"), None).await;
    mine.until(10, seen).await;
    assert_eq!(currents(&mine).len(), 2, "{:#?}", mine.frames);
}

/// A device is never told that its conversation's thread went out of its
/// reach (review W5-3): `current` answers `first` with the note any folder
/// without a current thread gets, and its feed's `folder.current` names no
/// previous thread and says `first`. The owner hears `gone`.
#[tokio::test]
async fn a_device_is_never_told_why_its_conversation_moved() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (first, _, _) = current_ok(&w, &d.client, folder, false).await;
    message(&w, first, 1).await;
    let mut devices = Feed::open(&w, &d.client, "", None).await;
    let mut mine = Feed::open(&w, &owner, "", None).await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{first}/settings"),
        json!({ "mcp_tools": serde_json::from_str::<Value>(LMGW).unwrap() }),
    )
    .await;
    assert_eq!(s, 200, "{v}");

    let (s, v) = current(&w, &d.client, folder, false).await;
    assert_eq!(s, 200, "{v}");
    let other = ongoing_folder(&w, &owner, "Other", 30, json!({})).await;
    let (_, _, plain_first) = current_ok(&w, &owner, other, false).await;
    assert_eq!(v["reason"], plain_first, "{v}");
    assert_eq!(
        v["note"],
        json!("folder 'Assistant' had no current thread"),
        "the note any folder without one gets"
    );
    let next = v["thread"]["id"].as_i64().unwrap();

    let moved = |f: &[crate::chat_feed::Frame]| {
        f.iter()
            .any(|f| f.event == "folder.current" && f.data["thread_id"] == next)
    };
    devices.until(10, moved).await;
    mine.until(10, moved).await;
    let seen = devices.named("folder.current").last().unwrap().data.clone();
    assert_eq!(
        (&seen["reason"], &seen["previous_thread_id"]),
        (&json!("first"), &json!(null)),
        "{seen}"
    );
    let told = mine.named("folder.current").last().unwrap().data.clone();
    assert_eq!(
        (&told["reason"], &told["previous_thread_id"]),
        (&json!("gone"), &json!(first)),
        "{told}"
    );
}

/// Review W6-15: an owner's rollover away from a current thread a device
/// cannot reach — `requested` here, `idle` alike — reads `first` with no
/// previous thread for the device, as every rollover from such a thread
/// does; the owner hears the reason it was.
#[tokio::test]
async fn a_rollover_from_a_hidden_thread_reads_first_for_a_device() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (hidden, _, _) = current_ok(&w, &owner, folder, false).await;
    message(&w, hidden, 1).await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{hidden}/settings"),
        json!({ "mcp_tools": serde_json::from_str::<Value>(LMGW).unwrap() }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let mut devices = Feed::open(&w, &d.client, "", None).await;
    let mut mine = Feed::open(&w, &owner, "", None).await;
    let (s, v) = current(&w, &owner, folder, true).await;
    assert_eq!((s, &v["reason"]), (200, &json!("requested")), "{v}");
    let next = v["thread"]["id"].as_i64().unwrap();
    let moved = |f: &[crate::chat_feed::Frame]| {
        f.iter()
            .any(|f| f.event == "folder.current" && f.data["thread_id"] == next)
    };
    devices.until(10, moved).await;
    mine.until(10, moved).await;
    let seen = devices.named("folder.current").last().unwrap().data.clone();
    assert_eq!(
        (&seen["reason"], &seen["previous_thread_id"]),
        (&json!("first"), &json!(null)),
        "{seen}"
    );
    let told = mine.named("folder.current").last().unwrap().data.clone();
    assert_eq!(
        (&told["reason"], &told["previous_thread_id"]),
        (&json!("requested"), &json!(hidden)),
        "{told}"
    );
}
