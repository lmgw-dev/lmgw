//! The self-admin toolset and a device (client-apps design L3, review
//! W3-1): a thread with `lmgw` among its tools, and a folder whose defaults
//! attach it, drive the self-admin plane as Admin Chat does, so for a device
//! they do not exist. Attaching the toolset to a thread a device holds takes
//! it out of the device's reach at once: its bound session closes, and its
//! feed hears the thread go. (A device writing `lmgw` itself is refused by
//! L5, `writes`.)

use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::{chat_thread, device_world, get, post};
use crate::chat_feed::Feed;

#[tokio::test]
async fn a_folder_whose_defaults_attach_the_self_admin_toolset_is_absent_for_a_device() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/folders",
        json!({ "name": "Ops", "defaults": { "mcp_tools": [{ "server_label": "lmgw" }] } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let ops = v["id"].as_i64().unwrap();
    let (_, v) = post(&w, &owner, "/chat/api/folders", json!({ "name": "Home" })).await;
    let home = v["id"].as_i64().unwrap();
    // A thread the owner starts there inherits the toolset, and is gone for
    // the device too.
    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "folder_id": ops }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let inherited = v["id"].as_i64().unwrap();

    let folder_ids = |v: &Value| -> Vec<i64> {
        v["folders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].as_i64().unwrap())
            .collect()
    };
    let (_, mine) = get(&w, &owner, "/chat/api/folders").await;
    assert_eq!(folder_ids(&mine), vec![ops, home]);
    let (_, theirs) = get(&w, &d.client, "/chat/api/folders").await;
    assert_eq!(folder_ids(&theirs), vec![home], "{theirs}");
    let (_, list) = get(&w, &d.client, "/chat/api/threads").await;
    assert_eq!(folder_ids(&list), vec![home], "{list}");
    let (s, _) = get(&w, &d.client, &format!("/chat/api/threads/{inherited}")).await;
    assert_eq!(s, 404);

    // Every folder route answers the device as for a folder that is not
    // there, and changes nothing.
    let mine_thread = chat_thread(&w, &d.client, "chatty").await;
    for (path, body) in [
        (
            format!("/chat/api/folders/{ops}"),
            json!({ "name": "Mine" }),
        ),
        (
            format!("/chat/api/folders/{ops}/delete"),
            json!({ "threads": "delete" }),
        ),
        (
            "/chat/api/threads".to_string(),
            json!({ "model_alias": "chatty", "folder_id": ops }),
        ),
        (
            format!("/chat/api/threads/{mine_thread}/move"),
            json!({ "folder_id": ops }),
        ),
    ] {
        let (s, v) = post(&w, &d.client, &path, body).await;
        assert_eq!((s, &v["code"]), (404, &json!("not_found")), "{path}: {v}");
    }
    let (s, _) = get(
        &w,
        &d.client,
        &format!("/chat/api/folders/{ops}/export?format=json"),
    )
    .await;
    assert_eq!(s, 404);
    let (_, still) = get(&w, &owner, "/chat/api/folders").await;
    assert_eq!(still["folders"][0]["name"], "Ops");
    let (s, _) = get(&w, &owner, &format!("/chat/api/threads/{inherited}")).await;
    assert_eq!(s, 200, "the device's delete reached nothing");
}

#[tokio::test]
async fn attaching_the_self_admin_toolset_takes_a_thread_out_of_a_device_s_reach_at_once() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let mut feed = Feed::open(&w, &d.client, "", None).await;
    let mut owners = Feed::open(&w, &owner, "", None).await;
    let bearer = format!("Bearer {}", d.key);
    let mut ws = w
        .connect(
            &format!("chat_thread={tid}"),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));

    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");

    // The bound session closes as a revocation of that thread.
    let close = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(c))) => break c,
                Some(Ok(_)) => continue,
                other => panic!("expected the close, got {other:?}"),
            }
        }
    })
    .await
    .expect("the session closes")
    .expect("a close frame with a reason");
    assert_eq!(u16::from(close.code), 4004, "out of reach, not revoked");
    // Neutral (review W4-18): the device is not told why.
    assert_eq!(
        close.reason.as_str(),
        format!("chat thread {tid} is out of reach for this key")
    );

    // The owner hears the session end as a revocation of the device's
    // reach (review W5-6, the W4-13 residue); the device hears nothing of it
    // — the thread is not there for it.
    owners
        .until(10, |f| f.iter().any(|f| f.event == "voice.ended"))
        .await;
    let ended = owners.named("voice.ended")[0].data.clone();
    assert_eq!(
        (&ended["reason"], &ended["by"]),
        (&json!("revoked"), &json!("device 'phone'")),
        "{ended}"
    );

    // The feed tells the device the thread is gone; by id it is not found.
    feed.until(10, |f| f.iter().any(|f| f.event == "thread.deleted"))
        .await;
    assert!(feed.named("voice.ended").is_empty(), "{:#?}", feed.frames);
    let gone = &feed.named("thread.deleted")[0].data;
    assert_eq!(
        (gone["thread_id"].as_i64(), &gone["deleted"]),
        (Some(tid), &json!(true))
    );
    let (s, _) = get(&w, &d.client, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(s, 404);

    // Taken off again, it comes back as a new thread for the device.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    feed.until(10, |f| {
        f.iter()
            .any(|f| f.event == "thread.created" && f.data["id"] == tid)
    })
    .await;
    let (s, _) = get(&w, &d.client, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(s, 200);
}

/// Review W4-7: a plain thread in a folder whose defaults take the toolset
/// stays the device's, in no folder — by id, in the list, in search, in an
/// export, in pin's and archive's answers (review W5-5) and in the feed —
/// and is back in it when the toolset goes.
#[tokio::test]
async fn a_plain_thread_in_a_folder_that_left_a_device_s_reach_is_in_none_for_it() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let (_, v) = post(&w, &owner, "/chat/api/folders", json!({ "name": "Home" })).await;
    let folder = v["id"].as_i64().unwrap();
    let tid = chat_thread(&w, &owner, "chatty").await;
    let (s, _) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/move"),
        json!({ "folder_id": folder }),
    )
    .await;
    assert_eq!(s, 200);
    crate::device_chat::seed(&w, tid, "zebra crossing").await;
    let mut feed = Feed::open(&w, &d.client, "", None).await;

    let set_defaults = |defaults: Value| {
        let (w, owner) = (&w, &owner);
        async move {
            let (s, v) = post(
                w,
                owner,
                &format!("/chat/api/folders/{folder}"),
                json!({ "defaults": defaults }),
            )
            .await;
            assert_eq!(s, 200, "{v}");
        }
    };
    set_defaults(json!({ "mcp_tools": [{ "server_label": "lmgw" }] })).await;

    let (s, v) = get(&w, &d.client, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(s, 200, "the thread itself is plain: {v}");
    assert_eq!(v["thread"]["folder_id"], json!(null));
    let (_, list) = get(&w, &d.client, "/chat/api/threads").await;
    let row = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == tid)
        .unwrap()
        .clone();
    assert_eq!(row["folder_id"], json!(null));
    let (_, found) = get(&w, &d.client, "/chat/api/search?q=zebra").await;
    assert_eq!(found["threads"][0]["thread_id"], tid, "{found}");
    assert_eq!(found["threads"][0]["folder_id"], json!(null));
    let (_, none) = get(
        &w,
        &d.client,
        &format!("/chat/api/search?q=zebra&folder={folder}"),
    )
    .await;
    assert_eq!(
        none["threads"],
        json!([]),
        "as for a folder that is not there"
    );
    let (s, export) = get(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/export?format=json"),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(export["folder"], json!(null), "{export}");
    assert!(!export.to_string().contains("Home"));
    // Pin and archive answer with the thread as the device reads it
    // (review W5-5).
    for (route, body) in [
        ("pin", json!({ "pinned": true })),
        ("pin", json!({ "pinned": false })),
        ("archive", json!({ "archived": true })),
        ("archive", json!({ "archived": false })),
    ] {
        let (s, v) = post(
            &w,
            &d.client,
            &format!("/chat/api/threads/{tid}/{route}"),
            body,
        )
        .await;
        assert_eq!(s, 200, "{route}: {v}");
        assert_eq!(v["folder_id"], json!(null), "{route}: {v}");
    }
    // The owner's view is unchanged.
    let (_, mine) = get(&w, &owner, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(mine["thread"]["folder_id"], folder);

    // The feed: the folder goes, the thread is re-rendered without it.
    feed.until(10, |f| {
        f.iter()
            .any(|f| f.event == "thread.updated" && f.data["id"] == tid)
    })
    .await;
    assert_eq!(feed.named("folder.deleted")[0].data["folder_id"], folder);
    let updated = feed
        .named("thread.updated")
        .into_iter()
        .find(|f| f.data["id"] == tid)
        .unwrap()
        .data
        .clone();
    assert_eq!(updated["folder_id"], json!(null), "{updated}");

    // Taken off: the folder and the thread's place in it are back.
    set_defaults(json!({})).await;
    feed.until(10, |f| f.iter().any(|f| f.event == "folder.created"))
        .await;
    // (The pin and archive above recorded updates of their own, rendered
    // without the folder while it was hidden.)
    feed.until(10, |f| {
        f.iter().any(|f| {
            f.event == "thread.updated" && f.data["id"] == tid && f.data["folder_id"] == folder
        })
    })
    .await;
}

/// Review W4-16: creating a thread in a hidden folder that names a model
/// answers as for a folder that is not there, whatever model the device
/// asks for — no oracle between the two.
#[tokio::test]
async fn a_thread_in_a_hidden_folder_is_refused_as_in_a_missing_one() {
    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let d = crate::device_chat::pair(
        &w,
        "phone",
        json!({ "scope_mode": "allow", "scope_patterns": "chatty" }),
    )
    .await;
    let (_, v) = post(
        &w,
        &w.gw.client(),
        "/chat/api/folders",
        json!({ "name": "Ops", "defaults": {
            "model_alias": "chatty", "mcp_tools": [{ "server_label": "lmgw" }] } }),
    )
    .await;
    let hidden = v["id"].as_i64().unwrap();
    for model in ["chatty", "other"] {
        let answer = |folder: i64| {
            let (w, d) = (&w, &d);
            async move {
                post(
                    w,
                    &d.client,
                    "/chat/api/threads",
                    json!({ "model_alias": model, "folder_id": folder }),
                )
                .await
            }
        };
        let (a, b) = (answer(hidden).await, answer(9_999).await);
        assert_eq!(a, b, "{model}");
        assert_eq!(a.0, 404, "{model}: {:?}", a.1);
    }
}

/// Review W5-18: a device's turn still running when the owner attaches the
/// self-admin toolset saves nothing into the thread. Since the pre-merge
/// review's P-8 the attach stops it at once, as a delete does: the turn ends
/// `superseded`, its upstream request dropped. The save's re-check of the
/// device's reach under the thread's lock stays behind it.
#[tokio::test]
async fn a_device_turn_running_at_the_attach_saves_nothing() {
    use std::sync::Arc;

    use crate::support::realtime_fakes::Turn;

    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let release = Arc::new(tokio::sync::Notify::new());
    w.chat.push(Turn::Held(
        release.clone(),
        Box::new(Turn::text(&["secret", "reply"])),
    ));
    let turn = {
        let w2 = (w.gw.to_string(), d.client.clone());
        tokio::spawn(async move {
            let (base, client) = w2;
            let resp = client
                .post(format!("{base}/chat/api/threads/{tid}/send"))
                .json(&json!({ "content": "hello" }))
                .send()
                .await
                .unwrap();
            crate::device_chat::frames(&resp.text().await.unwrap())
        })
    };
    for _ in 0..100 {
        if w.chat.seen.chat_count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(w.chat.seen.chat_count(), 1, "the turn reached its upstream");
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    release.notify_one();
    let frames = tokio::time::timeout(Duration::from_secs(10), turn)
        .await
        .expect("the turn ends")
        .unwrap();
    assert!(
        frames
            .iter()
            .any(|(event, data)| event == "error" && data["code"] == "superseded"),
        "{frames:?}"
    );
    let (_, v) = get(&w, &owner, &format!("/chat/api/threads/{tid}")).await;
    let roles: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user"], "no reply saved: {v}");
}
