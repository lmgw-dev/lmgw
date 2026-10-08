//! A device allowed lmgw's admin tools (`self_admin`, client-apps design
//! L3/L5, the owner's decision of 2026-10-07): it sees and uses the Chat
//! threads and folders that carry the self-admin toolset (`lmgw`), in text
//! and in voice, and its tool scope takes in the label; it attaches the
//! label itself only at `full` (`admin_attach`). Admin Chat stays hidden
//! from it. The switch is the owner's (`key_create`,
//! `key_set`); turned off, the device loses that reach at once — its feed
//! hears the threads and folders go, its bound sessions on them close with
//! the neutral 4004 — and turned on, its feed hears them come.

use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::{admin_thread, chat_thread, get, op, pair, post, rows_of, self_admin_thread, sse};
use crate::chat_feed::Feed;
use crate::realtime_chat_thread::{next, world, World};
use crate::support::realtime_fakes::{Step, Turn};

/// A folder of the owner's whose defaults attach the toolset: its id.
async fn toolset_folder(w: &World) -> i64 {
    let (s, v) = post(
        w,
        &w.gw.client(),
        "/chat/api/folders",
        json!({ "name": "Ops", "defaults": { "mcp_tools": [{ "server_label": "lmgw" }] } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    v["id"].as_i64().unwrap()
}

fn ids(v: &Value, key: &str) -> Vec<i64> {
    v[key]
        .as_array()
        .unwrap_or_else(|| panic!("no {key} in {v}"))
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect()
}

/// The close of `ws`, within 10 s.
async fn closed(ws: &mut crate::support::realtime_fakes::Ws) -> (u16, String) {
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
    (u16::from(close.code), close.reason.to_string())
}

#[tokio::test]
async fn a_device_allowed_the_admin_tools_sees_the_toolset_and_never_admin_chat() {
    let w = world(|_| {}).await;
    let phone = pair(&w, "phone", json!({})).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let tools = self_admin_thread(&w).await;
    let admin = admin_thread(&w).await;
    let folder = toolset_folder(&w).await;

    // By id: the toolset's thread is the allowed device's, Admin Chat no
    // device's.
    let (s, _) = get(
        &w,
        &phone.client,
        &format!("/chat/api/threads/{}", tools.id),
    )
    .await;
    assert_eq!(s, 404, "a device without the switch");
    let (s, v) = get(&w, &desk.client, &format!("/chat/api/threads/{}", tools.id)).await;
    assert_eq!(s, 200, "{v}");
    for d in [&phone, &desk] {
        let (s, _) = get(&w, &d.client, &format!("/chat/api/threads/{}", admin.id)).await;
        assert_eq!(s, 404, "Admin Chat stays hidden, switch or not");
    }

    // The lists and the folder counts.
    let (_, list) = get(&w, &desk.client, "/chat/api/threads").await;
    let threads = ids(&list, "threads");
    assert!(
        threads.contains(&tools.id) && !threads.contains(&admin.id),
        "{list}"
    );
    assert!(ids(&list, "folders").contains(&folder), "{list}");
    let (_, list) = get(&w, &phone.client, "/chat/api/threads").await;
    assert!(!ids(&list, "threads").contains(&tools.id), "{list}");
    assert!(!ids(&list, "folders").contains(&folder), "{list}");

    // L5: at read only it does not attach the toolset itself, even to a
    // thread of its own (the branch review's verification V-4: a thread
    // with the toolset steers the owner's later turns there); the other
    // device is not allowed the label at all.
    let mine = chat_thread(&w, &desk.client, "chatty").await;
    let attach = json!({ "mcp_tools": [{ "server_label": "lmgw" }] });
    let (s, v) = post(
        &w,
        &desk.client,
        &format!("/chat/api/threads/{mine}/settings"),
        attach.clone(),
    )
    .await;
    assert_eq!(
        (s, &v["code"]),
        (403, &json!("chat_toolset_needs_full")),
        "{v}"
    );
    let (_, t) = get(&w, &desk.client, &format!("/chat/api/threads/{mine}")).await;
    assert_eq!(
        t["thread"]["mcp_tools"],
        json!([]),
        "nothing was attached: {t}"
    );
    let theirs = chat_thread(&w, &phone.client, "chatty").await;
    let (s, v) = post(
        &w,
        &phone.client,
        &format!("/chat/api/threads/{theirs}/settings"),
        attach,
    )
    .await;
    assert_eq!(
        (s, &v["code"]),
        (403, &json!("tool_label_out_of_scope")),
        "{v}"
    );
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not allowed lmgw's admin tools"),
        "{v}"
    );

    // Its feed says it, and the key row does.
    let feed = Feed::open(&w, &desk.client, "", None).await;
    assert_eq!(feed.hello()["self_admin"], json!("read_only"));
    let feed = Feed::open(&w, &phone.client, "", None).await;
    assert_eq!(feed.hello()["self_admin"], json!("off"));
    let (_, keys) = get(&w, &w.gw.client(), "/api/usage/keys").await;
    let row = |id: i64| {
        keys["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(row(desk.id)["self_admin"], json!("read_only"));
    assert_eq!(row(phone.id)["self_admin"], json!("off"));
}

/// Its turn in a thread with the toolset runs the tools, as itself: the
/// model is offered `lmgw__*`, a call runs, and every model call is the
/// device's row. The owner attached the toolset to the device's thread: at
/// read only the device does not attach it itself.
#[tokio::test]
async fn a_device_allowed_the_admin_tools_runs_them_in_its_turn_as_itself() {
    let w = world(|_| {}).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let tid = chat_thread(&w, &desk.client, "chatty").await;
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    w.chat.push(Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id: Some("call_1"),
            name: "lmgw__status",
        },
        Step::CallArgs {
            index: 0,
            args: "{}",
        },
        Step::Finish("tool_calls"),
        Step::Usage(10, 5),
    ]));
    w.chat.push(Turn::text(&["All", " good."]));
    let before = w.chat.seen.chat_count();
    let (s, frames) = sse(
        &w,
        &desk.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "how is lmgw?" }),
    )
    .await;
    assert_eq!(s, 200);
    assert!(frames.iter().any(|(e, _)| e == "done"), "{frames:?}");
    let offered = w.chat.seen.chat(before).to_string();
    assert!(
        offered.contains("lmgw__status"),
        "the tools were offered: {offered}"
    );
    let answered = w.chat.seen.chat(before + 1);
    let roles: Vec<&str> = answered["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["role"].as_str())
        .collect();
    assert!(roles.contains(&"tool"), "the call ran: {answered}");
    let rows = rows_of(&w, desk.id).await;
    assert_eq!(
        rows.iter()
            .map(|r| (r.0.as_str(), r.1.as_str()))
            .collect::<Vec<_>>(),
        [
            ("chat", "chatty"),
            ("admin-tool", "lmgw__status"),
            ("chat", "chatty")
        ],
        "the model calls and the tool call are the device's: {rows:?}"
    );
}

/// With the self-admin level at full and the device's at full: the session
/// says its turns may change lmgw (`admin_tools`), as for the owner — the
/// levels still decide.
#[tokio::test]
async fn a_device_allowed_the_admin_tools_binds_a_toolset_thread_and_never_admin_chat() {
    let w = world(|s| s.self_admin = lmgw_core::config::SelfAdmin::Full).await;
    let phone = pair(&w, "phone", json!({})).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    let tools = self_admin_thread(&w).await;
    let admin = admin_thread(&w).await;
    let as_phone = format!("Bearer {}", phone.key);
    let as_desk = format!("Bearer {}", desk.key);
    let code = |v: &Value| v["error"]["code"].as_str().unwrap_or_default().to_string();

    let (s, v) = w
        .connect(
            &format!("chat_thread={}", tools.id),
            &[("authorization", as_phone.as_str())],
        )
        .await
        .unwrap_err();
    assert_eq!((s, code(&v)), (404, "chat_thread_not_found".into()), "{v}");
    let mut ws = w
        .connect(
            &format!("chat_thread={}", tools.id),
            &[("authorization", as_desk.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    let created = next(&mut ws).await;
    assert_eq!(created["type"], "session.created", "{created}");
    assert_eq!(
        created["session"]["lmgw"]["resolved"]["chat_thread"]["admin_tools"],
        json!(true),
        "{created}"
    );
    let (s, v) = w
        .connect(
            &format!("chat_thread={}", admin.id),
            &[("authorization", as_desk.as_str())],
        )
        .await
        .unwrap_err();
    assert_eq!((s, code(&v)), (404, "chat_thread_not_found".into()), "{v}");

    // Attaching or removing the toolset no longer moves the thread for it:
    // its session goes on.
    let (s, v) = post(
        &w,
        &w.gw.client(),
        &format!("/chat/api/threads/{}/settings", tools.id),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }], "system_prompt": "Be brief." }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    crate::support::realtime_fakes::send(&mut ws, json!({"type": "input_audio_buffer.clear"}))
        .await;
    let ev = next(&mut ws).await;
    assert_eq!(ev["type"], "input_audio_buffer.cleared", "{ev}");
}

/// Turned off, the device loses the toolset's reach at once: its session
/// bound to such a thread closes with the neutral 4004, and its feed hears
/// the thread, its plain thread in the toolset's folder and the folder go,
/// in a delete's order, then a fresh `state`. Turned on again, they come
/// back: the folder first.
#[tokio::test]
async fn the_switch_takes_the_toolset_s_threads_from_the_device_at_once_and_gives_them_back() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let tools = self_admin_thread(&w).await;
    let folder = toolset_folder(&w).await;
    let inside = chat_thread(&w, &owner, "chatty").await;
    let (s, _) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{inside}/move"),
        json!({ "folder_id": folder }),
    )
    .await;
    assert_eq!(s, 200);
    let admin = admin_thread(&w).await;
    let mut feed = Feed::open(&w, &desk.client, "", None).await;
    let bearer = format!("Bearer {}", desk.key);
    let mut ws = w
        .connect(
            &format!("chat_thread={}", tools.id),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut ws).await["type"], "session.created");

    let (s, v) = op(&w, "key_set", json!({ "id": desk.id, "self_admin": "off" })).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["changed"], json!(["self_admin"]), "{v}");
    assert_eq!(
        closed(&mut ws).await,
        (
            4004,
            format!("chat thread {} is out of reach for this key", tools.id)
        ),
        "neutral, as an attach closes it"
    );
    feed.until(10, |f| {
        f.iter().any(|f| f.event == "folder.deleted") && f.iter().any(|f| f.event == "state")
    })
    .await;
    // From the switch on (the bind's seed was a `thread.updated` before it).
    let from = feed
        .frames
        .iter()
        .position(|f| f.event == "thread.deleted")
        .expect("the thread went");
    let order: Vec<(String, i64)> = feed.frames[from..]
        .iter()
        .filter(|f| f.event.starts_with("thread.") || f.event.starts_with("folder."))
        .map(|f| (f.event.clone(), crate::chat_feed::subject(f)))
        .collect();
    assert_eq!(
        order,
        [
            ("thread.deleted".to_string(), tools.id),
            ("thread.updated".to_string(), inside),
            ("folder.deleted".to_string(), folder),
        ],
        "{:#?}",
        feed.frames
    );
    let moved = &feed.named("thread.updated")[0].data;
    assert_eq!(
        moved["thread"]["folder_id"],
        json!(null),
        "in no folder now"
    );
    assert!(
        feed.frames
            .iter()
            .all(|f| crate::chat_feed::subject(f) != admin.id),
        "Admin Chat is never named"
    );
    let (s, _) = get(&w, &desk.client, &format!("/chat/api/threads/{}", tools.id)).await;
    assert_eq!(s, 404);

    // On again: the folder, then the thread, then its plain thread in it.
    let seen = feed.frames.len();
    let (s, v) = op(
        &w,
        "key_set",
        json!({ "id": desk.id, "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    feed.until(10, |f| {
        f[seen..]
            .iter()
            .any(|f| f.event == "thread.updated" && crate::chat_feed::subject(f) == inside)
    })
    .await;
    let order: Vec<(String, i64)> = feed.frames[seen..]
        .iter()
        .filter(|f| f.event.starts_with("thread.") || f.event.starts_with("folder."))
        .map(|f| (f.event.clone(), crate::chat_feed::subject(f)))
        .collect();
    assert_eq!(
        order,
        [
            ("folder.created".to_string(), folder),
            ("thread.created".to_string(), tools.id),
            ("thread.updated".to_string(), inside),
        ],
        "{:#?}",
        &feed.frames[seen..]
    );
    let (s, _) = get(&w, &desk.client, &format!("/chat/api/threads/{}", tools.id)).await;
    assert_eq!(s, 200);
}

/// A device that was not connected hears the switch when it catches up
/// from its cursor: a `resync` at that point (the pre-merge review's P-1),
/// after which it reloads what it shows at the reach it has now — and
/// finds the toolset's thread.
#[tokio::test]
async fn a_device_away_at_the_switch_hears_it_when_it_catches_up() {
    let w = world(|_| {}).await;
    let phone = pair(&w, "phone", json!({})).await;
    let tools = self_admin_thread(&w).await;
    let feed = Feed::open(&w, &phone.client, "", None).await;
    let cursor = feed.cursor();
    drop(feed);
    let (s, v) = op(
        &w,
        "key_set",
        json!({ "id": phone.id, "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let mut back = Feed::open(&w, &phone.client, "", Some(&cursor)).await;
    assert_eq!(back.hello()["self_admin"], json!("read_only"));
    back.until(10, |f| f.iter().any(|f| f.event == "resync"))
        .await;
    let (_, list) = get(&w, &phone.client, "/chat/api/threads").await;
    assert!(ids(&list, "threads").contains(&tools.id), "{list}");
}

/// Only a device takes the switch, and only the owner sets it.
#[tokio::test]
async fn the_switch_is_a_device_s_and_the_owner_s_to_set() {
    let w = world(|_| {}).await;
    let (s, v) = op(
        &w,
        "key_create",
        json!({ "name": "ci", "kind": "client", "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 400, "{v}");
    let (s, v) = op(&w, "key_create", json!({ "name": "ci", "kind": "client" })).await;
    assert_eq!(s, 200, "{v}");
    let client = v["id"].as_i64().unwrap();
    let (s, v) = op(
        &w,
        "key_set",
        json!({ "id": client, "self_admin": "read_only" }),
    )
    .await;
    assert!(s >= 400, "{v}");
    assert!(v.to_string().contains("not a device key"), "{v}");
    let (s, v) = op(&w, "key_set", json!({ "id": client, "self_admin": "off" })).await;
    assert_eq!(
        (s, &v["changed"]),
        (200, &json!([])),
        "restating false is no attempt: {v}"
    );
    // A device cannot reach the key ops at all.
    let desk = pair(&w, "desktop", json!({})).await;
    let resp = desk
        .client
        .post(format!("{}/api/op/key_set", w.gw))
        .json(&json!({ "id": desk.id, "self_admin": "read_only" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    let (_, keys) = get(&w, &w.gw.client(), "/api/usage/keys").await;
    let row = keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == desk.id)
        .cloned()
        .unwrap();
    assert_eq!(row["self_admin"], json!("off"));
}

/// `current` and its rollover respect the switch (L3's attach rule): an
/// ongoing folder whose current thread took the toolset is the allowed
/// device's to continue; any other device asking gets a new thread.
#[tokio::test]
async fn current_and_its_rollover_respect_the_switch() {
    use crate::chat_ongoing::{current_ok, ongoing_folder};
    let w = world(|_| {}).await;
    let phone = pair(&w, "phone", json!({})).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 0, json!({})).await;
    let (first, _, _) = current_ok(&w, &owner, folder, false).await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{first}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (same, rolled, _) = current_ok(&w, &desk.client, folder, false).await;
    assert_eq!(
        (same, rolled),
        (first, false),
        "the allowed device continues it"
    );
    let (next, rolled, _) = current_ok(&w, &phone.client, folder, false).await;
    assert!(rolled && next != first, "another device gets a new thread");
}
