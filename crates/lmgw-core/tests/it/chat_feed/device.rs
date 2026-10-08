//! L3 in the feed (client-apps design §2.3): a device never receives an
//! event about an Admin Chat thread — live, in its catch-up, in `hello`, or
//! through a folder's counts — while the owner's feed carries everything.

use std::sync::Arc;

use serde_json::json;
use tokio::sync::Notify;

use super::{about_thread, Feed};
use crate::device_chat::{chat_thread, pair, post};
use crate::realtime_chat_thread::world;
use crate::support::realtime_fakes::{Step, Turn};

#[tokio::test]
async fn a_device_never_hears_of_an_admin_thread_live_or_in_its_catch_up() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let mut owner = Feed::open(&w, &w.gw.client(), "", None).await;
    let mut device = Feed::open(&w, &d.client, "", None).await;
    let start = device.cursor();

    let owner_client = w.gw.client();
    let (_, v) = post(
        &w,
        &owner_client,
        "/chat/api/folders",
        json!({ "name": "Ops" }),
    )
    .await;
    let folder = v["id"].as_i64().unwrap();
    let (s, v) = post(
        &w,
        &owner_client,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "kind": "admin", "folder_id": folder }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let admin = v["id"].as_i64().unwrap();
    post(
        &w,
        &owner_client,
        &format!("/chat/api/threads/{admin}/settings"),
        json!({ "system_prompt": "keys live here" }),
    )
    .await;
    post(
        &w,
        &owner_client,
        &format!("/chat/api/folders/{folder}"),
        json!({ "name": "Ops desk" }),
    )
    .await;
    post(
        &w,
        &owner_client,
        &format!("/chat/api/threads/{admin}/delete"),
        json!({}),
    )
    .await;
    // Something the device does see, last: once it has it, it has had its
    // chance at everything before.
    let visible = chat_thread(&w, &owner_client, "chatty").await;

    let seen = |f: &[super::Frame]| {
        f.iter()
            .any(|f| f.event == "thread.created" && f.data["id"] == visible)
    };
    owner.until(10, seen).await;
    device.until(10, seen).await;
    assert_eq!(
        about_thread(&owner.frames, admin).len(),
        3,
        "the owner hears of all three: {:#?}",
        owner.frames
    );
    assert!(
        about_thread(&device.frames, admin).is_empty(),
        "live: {:#?}",
        device.frames
    );
    // The folder is the device's too, but its counts leave Admin Chat out.
    let renamed = |frames: &[super::Frame]| {
        frames
            .iter()
            .find(|f| f.event == "folder.updated")
            .map(|f| f.data.clone())
            .unwrap()
    };
    assert_eq!(renamed(&device.frames)["name"], "Ops desk");
    // Rendered at delivery: by now the admin thread is deleted for both.
    assert_eq!(renamed(&device.frames)["threads_active"], 0);

    // In catch-up from before it all: the same, from the table.
    let mut caught = Feed::open(&w, &d.client, &format!("?since={start}"), None).await;
    caught.until(10, seen).await;
    assert!(
        about_thread(&caught.frames, admin).is_empty(),
        "catch-up: {:#?}",
        caught.frames
    );
    let mut owner_caught = Feed::open(&w, &w.gw.client(), &format!("?since={start}"), None).await;
    owner_caught.until(10, seen).await;
    assert_eq!(about_thread(&owner_caught.frames, admin).len(), 3);
}

#[tokio::test]
async fn a_folder_s_counts_and_the_live_turns_leave_admin_chat_out_for_a_device() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let owner_client = w.gw.client();
    let (_, v) = post(
        &w,
        &owner_client,
        "/chat/api/folders",
        json!({ "name": "Ops" }),
    )
    .await;
    let folder = v["id"].as_i64().unwrap();
    let (_, v) = post(
        &w,
        &owner_client,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "kind": "admin", "folder_id": folder }),
    )
    .await;
    let admin = v["id"].as_i64().unwrap();

    // An Admin Chat turn held mid-answer: live while the device connects.
    let hold = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Checking"),
        Step::Wait(hold.clone()),
        Step::Text(" done."),
        Step::Finish("stop"),
        Step::Usage(5, 3),
    ]));
    let mut owner = Feed::open(&w, &owner_client, "", None).await;
    let send = owner_client
        .post(format!("{}/chat/api/threads/{admin}/send", w.gw))
        .json(&json!({ "content": "status?" }))
        .send()
        .await
        .unwrap();
    assert_eq!(send.status(), 200);
    owner
        .until(10, |f| f.iter().any(|f| f.event == "turn.started"))
        .await;
    assert_eq!(owner.named("turn.started")[0].data["thread_id"], admin);

    let mut device = Feed::open(&w, &d.client, "", None).await;
    assert_eq!(device.hello()["turns"], json!([]), "{}", device.hello());
    let owner_now = Feed::open(&w, &owner_client, "", None).await;
    assert_eq!(owner_now.hello()["turns"][0]["thread_id"], admin);

    // The folder as the device lists it, in a stored event: no admin
    // thread counted. The owner's rendering counts it.
    post(
        &w,
        &owner_client,
        &format!("/chat/api/folders/{folder}"),
        json!({ "name": "Ops desk" }),
    )
    .await;
    device
        .until(10, |f| f.iter().any(|f| f.event == "folder.updated"))
        .await;
    assert_eq!(device.named("folder.updated")[0].data["threads_active"], 0);
    owner
        .until(10, |f| f.iter().any(|f| f.event == "folder.updated"))
        .await;
    assert_eq!(owner.named("folder.updated")[0].data["threads_active"], 1);

    // The admin turn ends: the owner hears it, the device does not; the
    // device's next event is a chat thread's turn.
    hold.notify_one();
    drop(send);
    owner
        .until(10, |f| f.iter().any(|f| f.event == "turn.done"))
        .await;
    let chat = chat_thread(&w, &d.client, "chatty").await;
    let (s, _) = crate::device_chat::sse(
        &w,
        &d.client,
        &format!("/chat/api/threads/{chat}/send"),
        json!({ "content": "hi" }),
    )
    .await;
    assert_eq!(s, 200);
    device
        .until(10, |f| f.iter().any(|f| f.event == "turn.done"))
        .await;
    let turns: Vec<_> = device
        .frames
        .iter()
        .filter(|f| f.event.starts_with("turn."))
        .map(|f| f.data["thread_id"].as_i64().unwrap())
        .collect();
    assert_eq!(turns, vec![chat, chat], "{:#?}", device.frames);
    assert!(about_thread(&device.frames, admin).is_empty());
}

/// Reviews W4-8, W4-13: the self-admin toolset attached to a thread while
/// an owner's turn runs in it, then taken off — a device's feed hears the
/// thread go and come, and a fresh `state` each time, so the running turn
/// leaves its view and comes back, and its `turn.done` is one the device
/// was told is running.
#[tokio::test]
async fn a_toolset_flip_mid_turn_gives_a_device_a_fresh_state_each_way() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let owner_client = w.gw.client();
    let tid = chat_thread(&w, &owner_client, "chatty").await;
    let hold = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Thinking"),
        Step::Wait(hold.clone()),
        Step::Text(" done."),
        Step::Finish("stop"),
        Step::Usage(5, 3),
    ]));
    let mut device = Feed::open(&w, &d.client, "", None).await;
    let mut owner = Feed::open(&w, &owner_client, "", None).await;
    let send = owner_client
        .post(format!("{}/chat/api/threads/{tid}/send", w.gw))
        .json(&json!({ "content": "hello" }))
        .send()
        .await
        .unwrap();
    assert_eq!(send.status(), 200);
    device
        .until(10, |f| f.iter().any(|f| f.event == "turn.started"))
        .await;

    let set_tools = |tools: serde_json::Value| {
        let (w, owner_client) = (&w, &owner_client);
        async move {
            let (s, v) = post(
                w,
                owner_client,
                &format!("/chat/api/threads/{tid}/settings"),
                json!({ "mcp_tools": tools }),
            )
            .await;
            assert_eq!(s, 200, "{v}");
        }
    };
    let states = |f: &[super::Frame]| f.iter().filter(|f| f.event == "state").count();
    set_tools(json!([{ "server_label": "lmgw" }])).await;
    device.until(10, |f| states(f) >= 1).await;
    let gone = device.named("state")[0].data.clone();
    assert_eq!(gone["turns"], json!([]), "{gone}");
    // Neutral (review W6-4): nothing says why.
    assert_eq!(
        gone["reason"],
        json!("this is the live state now"),
        "{gone}"
    );
    assert!(device.frames.iter().any(|f| f.event == "thread.deleted"));

    set_tools(json!([])).await;
    device.until(10, |f| states(f) >= 2).await;
    let back = device.named("state")[1].data.clone();
    assert_eq!(back["turns"][0]["thread_id"], tid, "{back}");

    hold.notify_one();
    drop(send);
    device
        .until(10, |f| f.iter().any(|f| f.event == "turn.done"))
        .await;
    assert_eq!(device.named("turn.done")[0].data["thread_id"], tid);
    // The owner's feed needed no refresh.
    owner
        .until(10, |f| f.iter().any(|f| f.event == "turn.done"))
        .await;
    assert_eq!(states(&owner.frames), 0, "{:#?}", owner.frames);
}

/// Review W4-15: records a device may not see move its cursor all the same.
/// The keep-alive hands it the moved cursor (an `id:` with no data), so a
/// reconnect after a long stretch of them resumes without a `resync`.
#[tokio::test]
async fn the_keep_alive_hands_a_device_the_cursor_its_skipped_records_moved() {
    let w = world(|_| {}).await;
    super::set(&w, |s| s.chat_feed_keepalive_s = 1).await;
    let d = pair(&w, "phone", json!({})).await;
    let mut device = Feed::open(&w, &d.client, "", None).await;
    let (s, v) = post(
        &w,
        &w.gw.client(),
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "kind": "admin" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let head = lmgw_core::store::feed::bounds(&w.state.db)
        .await
        .unwrap()
        .head;
    device
        .until(10, |f| f.iter().any(|f| f.event == ":" && f.id.is_some()))
        .await;
    let tick = device
        .frames
        .iter()
        .find(|f| f.event == ":" && f.id.is_some())
        .unwrap();
    assert_eq!(
        tick.id.as_deref().and_then(|c| c.split(':').nth(1)),
        Some(head.to_string().as_str())
    );
    assert!(device.stored().is_empty(), "{:#?}", device.frames);
    // Resumed from it: no resync, nothing about the admin thread.
    let mut again = Feed::open(&w, &d.client, "", Some(tick.id.as_deref().unwrap())).await;
    again.until(3, |f| f.len() >= 2).await;
    assert!(again.named("resync").is_empty(), "{:#?}", again.frames);

    // Review W6-3: the tag the device was handed for the record it may not
    // see is the record's own random draw, stored with it, and no function
    // of what the record says; `hello.cursor` at that hidden head carries
    // the same.
    let stored = lmgw_core::store::feed::record_at(&w.state.db, head)
        .await
        .unwrap()
        .unwrap();
    let tag = tick
        .id
        .as_deref()
        .and_then(|c| c.split(':').nth(2))
        .unwrap();
    assert_eq!(tag, stored.tag);
    assert!(
        tag.len() == 16 && tag.bytes().all(|b| b.is_ascii_hexdigit()),
        "{tag}"
    );
    let fresh = Feed::open(&w, &d.client, "", None).await;
    let epoch = fresh.hello()["epoch"].as_str().unwrap().to_string();
    assert_eq!(
        fresh.hello()["cursor"],
        json!(format!("{epoch}:{head}:{tag}"))
    );
    // Two records with the same facts draw two tags.
    for _ in 0..2 {
        let mut conn = w.state.db.acquire().await.unwrap();
        lmgw_core::store::feed::record(
            &mut conn,
            lmgw_core::store::feed::Change {
                kind: "thread.updated",
                thread_id: Some(77),
                admin: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    let a = lmgw_core::store::feed::record_at(&w.state.db, head + 1)
        .await
        .unwrap()
        .unwrap();
    let b = lmgw_core::store::feed::record_at(&w.state.db, head + 2)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(a.tag, b.tag);
    assert_eq!((a.tag.len(), b.tag.len()), (16, 16));
}

/// A stored settings blob that does not parse is the defaults, in the
/// published snapshot and in the store's reading of the gateway's level
/// alike, so a device's feed opens at once, `hello` first. Read as one
/// field, a blob that held a valid level other than the default disagreed
/// with the snapshot for good: every device's feed waited a full keep-alive
/// before it opened at the store's level. A blob that was not JSON at all
/// failed the open.
#[tokio::test]
async fn a_malformed_settings_blob_opens_a_device_s_feed_at_once() {
    use lmgw_core::config::{SelfAdmin, Settings};
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({ "self_admin": "read_only" })).await;
    let default = Settings::default().self_admin;
    let other = if default == SelfAdmin::Full {
        "off"
    } else {
        "full"
    };
    for blob in [
        format!(r#"{{"self_admin": "{other}", "chat_feed_keepalive_s": "sixty"}}"#),
        format!(r#"{{"self_admin": "{other}", "#),
    ] {
        sqlx::query("UPDATE settings SET value = ?1 WHERE key = 'settings'")
            .bind(&blob)
            .execute(&w.state.db)
            .await
            .unwrap();
        w.state.reload_snapshot().await.unwrap();
        assert_eq!(w.state.snapshot().settings.self_admin, default, "{blob}");
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            d.client.get(format!("{}/chat/api/feed", w.gw)).send(),
        )
        .await
        .expect("the response starts")
        .unwrap();
        assert_eq!(resp.status(), 200, "{blob}");
        let mut feed = Feed::reading(resp);
        feed.until(5, |f| !f.is_empty()).await;
        assert_eq!(
            feed.frames[0].event, "hello",
            "{blob}: no wait for a publish: {:#?}",
            feed.frames
        );
    }
}
