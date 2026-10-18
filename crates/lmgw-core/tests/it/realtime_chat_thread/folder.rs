//! `?chat_folder=<id>[&new=true]` on the WebSocket (realtime calls design
//! §2.2, C23): the bind decides an ongoing folder's current thread by
//! `current`'s rules and binds it in one step under the folder's lock — the
//! current thread kept, or a new one made (first, idle, `new`), recorded as
//! `folder.current` on the feed — reported in
//! `session.lmgw.resolved.chat_thread`; the folder's refusals in the
//! handshake's shape (a folder out of a device's reach the 404 of a missing
//! one), the parameter rules, a rollover out of a device's scope making
//! nothing, two binds at once getting one thread, and `error.held_by` on the
//! 409 (§2.8). A kept thread that leaves the folder between the check and
//! the bind is the unit tests' (`realtime::thread::bind::tests`): no socket
//! reaches between the two.

use serde_json::{json, Value};

use super::{world, World};
use crate::chat_feed::Feed;
use crate::chat_ongoing::{current_ok, folder_of, message, ongoing_folder};
use crate::device_chat::{pair, post};
use crate::support::realtime_fakes::{next_event, Ws};

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

/// A session bound with `query` and `headers`: the socket and the thread
/// its `session.created` names.
async fn bound(w: &World, query: &str, headers: &[(&str, &str)]) -> (Ws, i64) {
    bound_at(w, &w.addr(), query, headers).await
}

/// [`bound`] at `addr`: a paired device's proxy to the device listener.
async fn bound_at(w: &World, addr: &str, query: &str, headers: &[(&str, &str)]) -> (Ws, i64) {
    let mut ws = w
        .connect_at(addr, query, headers)
        .await
        .unwrap_or_else(|(s, b)| panic!("{query}: expected a 101, got {s}: {b}"));
    let created = next_event(&mut ws).await;
    assert_eq!(created["type"], "session.created", "{created}");
    let id = created["session"]["lmgw"]["resolved"]["chat_thread"]["id"]
        .as_i64()
        .unwrap_or_else(|| panic!("no chat_thread in {created}"));
    (ws, id)
}

/// [`bound`] with the dashboard's cookie.
async fn by_cookie(w: &World, query: &str) -> (Ws, i64) {
    let cookie = w.cookie();
    bound(w, query, &[("cookie", &cookie)]).await
}

/// The folder's current thread, as its JSON says.
async fn current_of(w: &World, folder: i64) -> Value {
    folder_of(w, &w.gw.client(), folder).await.unwrap()["ongoing"]["current_thread_id"].clone()
}

/// The `folder.current` records about `folder` read so far.
fn records(feed: &Feed, folder: i64) -> Vec<Value> {
    feed.named("folder.current")
        .into_iter()
        .filter(|f| f.data["folder_id"] == folder)
        .map(|f| f.data.clone())
        .collect()
}

/// The threads in `folder`.
async fn threads_in(w: &World, folder: i64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM chat_threads WHERE folder_id = ?1")
        .bind(folder)
        .fetch_one(&w.state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_folder_binds_its_current_thread_and_starts_the_first_one() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({ "temperature": 0.3 })).await;
    let mut feed = Feed::open(&w, &owner, "", None).await;

    // No current thread yet: the bind makes it, from the folder's defaults.
    let (ws, first) = by_cookie(&w, &format!("chat_folder={folder}")).await;
    assert_eq!(current_of(&w, folder).await, json!(first));
    let t = w.get(&format!("/chat/api/threads/{first}")).await;
    assert_eq!(
        (
            &t["thread"]["folder_id"],
            &t["thread"]["model_alias"],
            &t["thread"]["temperature"]
        ),
        (&json!(folder), &json!("chatty"), &json!(0.3)),
        "{t}"
    );
    feed.until(10, |f| {
        f.iter()
            .any(|f| f.event == "folder.current" && f.data["folder_id"] == folder)
    })
    .await;
    assert_eq!(
        records(&feed, folder),
        vec![
            json!({"folder_id": folder, "thread_id": first, "previous_thread_id": null,
                    "reason": "first", "by": "the dashboard"})
        ]
    );
    drop(ws);

    // Again, with a message in it: the same thread, nothing started.
    message(&w, first, 1).await;
    let (_ws, again) = by_cookie(&w, &format!("chat_folder={folder}")).await;
    assert_eq!(again, first);
    assert_eq!(threads_in(&w, folder).await, 1);
}

#[tokio::test]
async fn new_and_idleness_roll_over_and_an_empty_thread_is_kept() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (first, _, _) = current_ok(&w, &owner, folder, false).await;
    let mut feed = Feed::open(&w, &owner, "", None).await;

    // Empty: kept under new=true, as `current` keeps it.
    let (ws, kept) = by_cookie(&w, &format!("chat_folder={folder}&new=true")).await;
    assert_eq!(kept, first);
    drop(ws);

    // With a message: new=true starts one.
    message(&w, first, 1).await;
    let (ws, requested) = by_cookie(&w, &format!("chat_folder={folder}&new=true")).await;
    assert_ne!(requested, first);
    assert_eq!(current_of(&w, folder).await, json!(requested));
    drop(ws);

    // Idle past the folder's 30 minutes: a new one without asking.
    message(&w, requested, 45).await;
    let (_ws, idle) = by_cookie(&w, &format!("chat_folder={folder}")).await;
    assert_ne!(idle, requested);
    assert_eq!(current_of(&w, folder).await, json!(idle));

    feed.until(10, |f| {
        f.iter()
            .filter(|f| f.event == "folder.current" && f.data["folder_id"] == folder)
            .count()
            >= 2
    })
    .await;
    assert_eq!(
        records(&feed, folder),
        vec![
            json!({"folder_id": folder, "thread_id": requested, "previous_thread_id": first,
                   "reason": "requested", "by": "the dashboard"}),
            json!({"folder_id": folder, "thread_id": idle, "previous_thread_id": requested,
                   "reason": "idle", "by": "the dashboard"}),
        ]
    );
}

#[tokio::test]
async fn the_folder_s_refusals_and_the_parameter_rules_come_before_the_101() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let cookie = w.cookie();
    let with = [("cookie", cookie.as_str())];
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let tid = w.thread("chatty", json!({})).await;

    let refused = |q: String| {
        let w = &w;
        let with = &with;
        async move { w.connect(&q, with).await.unwrap_err() }
    };

    let (status, body) = refused("chat_folder=9999".into()).await;
    assert_eq!(
        (status, code(&body)),
        (404, "chat_folder_not_found"),
        "{body}"
    );
    assert_eq!(body["error"]["message"], "there is no chat folder 9999");
    assert_eq!(body["error"]["type"], "invalid_request_error");

    let (_, v) = post(&w, &owner, "/chat/api/folders", json!({ "name": "Plain" })).await;
    let plain = v["id"].as_i64().unwrap();
    let (status, body) = refused(format!("chat_folder={plain}")).await;
    assert_eq!((status, code(&body)), (409, "not_ongoing"), "{body}");

    let bare = ongoing_folder(&w, &owner, "Bare", 30, json!({})).await;
    sqlx::query("UPDATE chat_folders SET defaults = '{}' WHERE id = ?1")
        .bind(bare)
        .execute(&w.state.db)
        .await
        .unwrap();
    let (status, body) = refused(format!("chat_folder={bare}")).await;
    assert_eq!((status, code(&body)), (409, "folder_no_model"), "{body}");
    assert_eq!(threads_in(&w, bare).await, 0, "nothing was made");

    for query in [
        format!("chat_folder={folder}&chat_thread={tid}"),
        "new=true".to_string(),
        format!("chat_thread={tid}&new=false"),
        "takeover=never".to_string(),
    ] {
        let (status, body) = refused(query.clone()).await;
        assert_eq!(
            (status, code(&body)),
            (400, "invalid_value"),
            "{query}: {body}"
        );
    }
    for query in [
        format!("chat_folder={folder}&takeover=ask"),
        format!("chat_thread={tid}&takeover=ask"),
    ] {
        let (status, body) = refused(query.clone()).await;
        assert_eq!(
            (status, code(&body)),
            (400, "invalid_value"),
            "{query}: {body}"
        );
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("takeover=ask applies to WebRTC calls"),
            "{body}"
        );
    }
    let (status, body) = refused(format!("model=chatty&chat_folder={folder}")).await;
    assert_eq!((status, code(&body)), (400, "owned_by_thread"), "{body}");

    // A client key: no chat capability, so nothing about the folder.
    sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES ('client', ?1, 1)")
        .bind(lmgw_core::config::hash_api_key("lmgw-client-key"))
        .execute(&w.state.db)
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let (status, body) = w
        .connect(
            &format!("chat_folder={folder}"),
            &[("authorization", "Bearer lmgw-client-key")],
        )
        .await
        .unwrap_err();
    assert_eq!(
        (status, code(&body)),
        (403, "chat_thread_not_allowed"),
        "{body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("(chat_folder)"),
        "{body}"
    );
    assert_eq!(
        current_of(&w, folder).await,
        json!(null),
        "no refusal made a thread"
    );
}

/// The folder's lock covers the decision and the bind: binds by folder at
/// once, from the dashboard and two devices, all bind one thread.
#[tokio::test]
async fn binds_by_folder_at_once_bind_one_thread() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let phone = pair(&w, "phone", json!({})).await;
    let desktop = pair(&w, "desktop", json!({})).await;
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let cookie = w.cookie();
    let query = format!("chat_folder={folder}");
    let ha = [("cookie", cookie.as_str())];
    let (a, b, c) = tokio::join!(
        bound(&w, &query, &ha),
        bound_at(&w, &phone.addr, &query, &[]),
        bound_at(&w, &desktop.addr, &query, &[]),
    );
    assert_eq!((a.1, b.1), (c.1, c.1), "one thread for every bind");
    assert_eq!(current_of(&w, folder).await, json!(c.1));
    assert_eq!(threads_in(&w, folder).await, 1);
}

/// §2.8, §8.4: the 409 names the holder as a field, exactly as
/// `voice.bound.by` names it; the message stays as it was.
#[tokio::test]
async fn takeover_never_names_the_holder_in_held_by() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let phone = pair(&w, "phone", json!({})).await;
    let desktop = pair(&w, "desktop", json!({})).await;
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;

    // The phone binds the folder's thread; the desktop's rebind by folder is
    // refused, naming it.
    let (_held, tid) = bound_at(&w, &phone.addr, &format!("chat_folder={folder}"), &[]).await;
    for query in [
        format!("chat_folder={folder}&takeover=never"),
        format!("chat_thread={tid}&takeover=never"),
    ] {
        let (status, body) = w.connect_as(&desktop, &query).await.unwrap_err();
        assert_eq!((status, code(&body)), (409, "chat_thread_bound"), "{body}");
        assert_eq!(body["error"]["held_by"], "device 'phone'", "{body}");
        assert_eq!(
            body["error"]["message"],
            "voice is in use on device 'phone': this bind asked not to take it over \
             (takeover=never)"
        );
    }
    assert_eq!(current_of(&w, folder).await, json!(tid), "nothing moved");

    // The dashboard holds another: "the dashboard".
    let other = w.thread("chatty", json!({})).await;
    let (_dash, _) = by_cookie(&w, &format!("chat_thread={other}")).await;
    let (status, body) = w
        .connect_as(&phone, &format!("chat_thread={other}&takeover=never"))
        .await
        .unwrap_err();
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["held_by"], "the dashboard", "{body}");
    // No other refusal carries it.
    let (_, body) = w.connect_as(&phone, "chat_thread=9999").await.unwrap_err();
    assert!(body["error"].get("held_by").is_none(), "{body}");
}

/// A folder out of a device's reach (defaults that attach the self-admin
/// toolset, the device not allowed lmgw's admin tools) is the 404 of a
/// folder that is not there, word for word but the id (client-apps L3).
#[tokio::test]
async fn a_device_gets_the_404_of_a_missing_folder_for_one_it_cannot_see() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let phone = pair(&w, "phone", json!({})).await;
    let hidden = ongoing_folder(
        &w,
        &owner,
        "Ops",
        30,
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    let (mine, _, _) = current_ok(&w, &owner, hidden, false).await;
    let (status, missing) = w.connect_as(&phone, "chat_folder=9999").await.unwrap_err();
    assert_eq!(status, 404, "{missing}");
    for query in [
        format!("chat_folder={hidden}"),
        format!("chat_folder={hidden}&new=true"),
    ] {
        let (status, body) = w.connect_as(&phone, &query).await.unwrap_err();
        assert_eq!(status, 404, "{query}: {body}");
        let mut expected = missing.clone();
        expected["error"]["message"] = json!(format!("there is no chat folder {hidden}"));
        assert_eq!(body, expected, "{query}");
    }
    // Nothing moved for the owner.
    assert_eq!(current_of(&w, hidden).await, json!(mine));
    assert_eq!(threads_in(&w, hidden).await, 1);
}

/// A device binds as its key: a rollover's thread is checked against the
/// key's scope before it is made, so a refused bind makes no thread and
/// records no `folder.current`.
#[tokio::test]
async fn a_rollover_out_of_a_device_s_scope_makes_nothing() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    // ASR and TTS in scope, the folder's chat model (`chatty`) not.
    let phone = pair(
        &w,
        "phone",
        json!({ "scope_mode": "allow", "scope_patterns": "hear\nspeak" }),
    )
    .await;
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (status, body) = w
        .connect_as(&phone, &format!("chat_folder={folder}"))
        .await
        .unwrap_err();
    assert_eq!((status, code(&body)), (403, "key_scope"), "{body}");
    assert!(body.to_string().contains("chatty"), "{body}");
    assert_eq!(current_of(&w, folder).await, json!(null));
    assert_eq!(threads_in(&w, folder).await, 0, "no thread was made");
    let recorded: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_feed WHERE type = 'folder.current' AND folder_id = ?1",
    )
    .bind(folder)
    .fetch_one(&w.state.db)
    .await
    .unwrap();
    assert_eq!(recorded, 0, "no folder.current");
}
