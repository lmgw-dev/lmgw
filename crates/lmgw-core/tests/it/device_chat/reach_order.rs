//! The close of a device's bound session whose thread leaves the device's
//! reach, and what the device reads once it heard it (client-apps design
//! L3; §1.6's close-code note, 2026-10-07). The close follows the write
//! that moved the thread: a device that asks for its folder's current
//! thread the moment it hears the 4004 gets the new one, and binds it. The
//! desktop client's live check found the close of an attach ahead of the
//! attach's commit, and `current` still naming the thread it had lost.
//!
//! A deleted thread closes the session exactly as a thread hidden from the
//! device does (review W4-18: a device cannot tell the two apart), its feed
//! hears the same, and so does a spoken turn in flight when it went.
//!
//! On a file database in WAL mode, as the gateway runs it: a test holds the
//! store's write lock to keep a write's commit back, as another write in
//! progress would, while reads go on. The session must stay open until the
//! write landed. A level's record reaches the device's feed only once the
//! snapshot that says it is published, too.

use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::StreamExt;
use lmgw_core::config::DeviceAdmin;
use lmgw_core::state::AppState;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::{chat_thread, get, pair, post, Device};

mod hangup;
mod stalled;
use crate::chat_feed::{Feed, Frame};
use crate::chat_ongoing::{current, current_ok, message, ongoing_folder};
use crate::realtime_chat_thread::{
    next, of_type, say, settings, until, until_type, world_on, World,
};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::{send, Step, Turn, Ws};

/// A world on a file database (WAL, a pool of connections: a read goes on
/// while a write waits for the lock), with the device `phone` paired at
/// `policy`. The directory goes with the test.
async fn file_world(policy: Value) -> (World, Device, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = lmgw_core::store::open(&dir.path().join("lmgw.sqlite"))
        .await
        .unwrap();
    let w = world_on(AppState::init_for_tests_on(db).await.unwrap(), |_| {}).await;
    let d = pair(&w, "phone", policy).await;
    (w, d, dir)
}

/// A session bound to `tid` with `auth`, past its `session.created`.
async fn bind_with(w: &World, auth: (&str, &str), tid: i64) -> Ws {
    let mut ws = w
        .connect(&format!("chat_thread={tid}"), &[auth])
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101 for thread {tid}, got {s}: {b}"));
    let created = next(&mut ws).await;
    assert_eq!(created["type"], "session.created", "{created}");
    ws
}

/// A session bound to `tid` as device `d`, and the watch its end is known
/// by ([`settle`]).
async fn bind(w: &World, d: &Device, tid: i64) -> (Ws, Links) {
    let bearer = format!("Bearer {}", d.key);
    let ws = bind_with(w, ("authorization", bearer.as_str()), tid).await;
    // After its `session.created`: the link's opening stamp is behind.
    let links = Links {
        rx: w.state.telemetry.subscribe(),
        key_id: d.id,
    };
    (ws, links)
}

/// The gateway's telemetry, watched for device key `key_id`'s link events.
struct Links {
    rx: tokio::sync::broadcast::Receiver<lmgw_core::telemetry::Event>,
    key_id: i64,
}

/// How a session ended: what it said after the change — every event but
/// the model states the connect warm sends when it sends them — and its
/// close.
#[derive(Debug, Clone, PartialEq)]
struct Ended {
    /// `(type, error code, error message)`.
    said: Vec<(String, String, String)>,
    code: u16,
    reason: String,
}

impl Ended {
    /// The same with thread `tid` named `N`: the sequences of two threads
    /// compared.
    fn of_any_thread(&self, tid: i64) -> Self {
        let named = format!("chat thread {tid} ");
        let n = |s: &str| s.replace(&named, "chat thread N ");
        Self {
            said: self
                .said
                .iter()
                .map(|(t, c, m)| (t.clone(), c.clone(), n(m)))
                .collect(),
            code: self.code,
            reason: n(&self.reason),
        }
    }
}

/// The session's end, when it closes within `within`; `None` when it is
/// still open then.
async fn ended(ws: &mut Ws, within: Duration) -> Option<Ended> {
    let mut said = Vec::new();
    let read = tokio::time::timeout(within, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(c))) => break c,
                Some(Ok(Message::Text(t))) => {
                    let ev: Value = serde_json::from_str(t.as_str()).unwrap();
                    if ev["type"] != "lmgw.model.state" {
                        said.push((
                            ev["type"].as_str().unwrap_or("").to_string(),
                            ev["error"]["code"].as_str().unwrap_or("").to_string(),
                            ev["error"]["message"].as_str().unwrap_or("").to_string(),
                        ));
                    }
                }
                Some(Ok(_)) => continue,
                other => panic!("expected the close, got {other:?}"),
            }
        }
    })
    .await;
    let frame = read.ok()?.expect("a close frame with a reason");
    Some(Ended {
        said,
        code: u16::from(frame.code),
        reason: frame.reason.to_string(),
    })
}

/// Wait for a device's session that closed to be gone: its socket ends,
/// and the `last_seen_at` stamp its link writes on its own task as it ends
/// has landed — said by the link's telemetry event, which follows the
/// stamp. A write of the test's own that met that stamp could be refused
/// `database is locked` at once (a deferred transaction on the file
/// database does not always wait for the lock), which is not what these
/// tests are about.
async fn settle(ws: &mut Ws, links: &mut Links) {
    use tokio::sync::broadcast::error::RecvError;
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(_)) = ws.next().await {}
        loop {
            match links.rx.recv().await {
                Ok(lmgw_core::telemetry::Event::Keys(k))
                    if k.key_id == links.key_id && k.what == "link" =>
                {
                    break;
                }
                Ok(_) | Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "the session's end was not stamped within 10 s"
    );
}

/// Every event `ws` sends within `within` but the connect warm's model
/// states — a close as `{"type": "close"}`: what a device hears while a
/// write waits for its commit.
async fn quiet(ws: &mut Ws, within: Duration) -> Vec<Value> {
    let mut said = Vec::new();
    let _ = tokio::time::timeout(within, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    let ev: Value = serde_json::from_str(t.as_str()).unwrap();
                    if ev["type"] != "lmgw.model.state" {
                        said.push(ev);
                    }
                }
                Some(Ok(Message::Close(c))) => {
                    said.push(json!({ "type": "close", "close": format!("{c:?}") }));
                    break;
                }
                Some(Ok(_)) => continue,
                _ => break,
            }
        }
    })
    .await;
    said
}

/// The store's write lock, held as another write in progress holds it: a
/// write that starts now waits for it, and reads go on.
async fn hold_writes(w: &World) -> sqlx::Transaction<'static, sqlx::Sqlite> {
    w.state.db.begin_with("BEGIN IMMEDIATE").await.unwrap()
}

/// A `POST` as `client`, run on its own task: its status.
fn post_later(
    w: &World,
    client: &reqwest::Client,
    path: &str,
    body: Value,
) -> tokio::task::JoinHandle<u16> {
    let req = client.post(format!("{}{path}", w.gw)).json(&body);
    tokio::spawn(async move { req.send().await.unwrap().status().as_u16() })
}

/// How long a session is watched for a close that must not come: one that
/// came before a commit held back comes within a few milliseconds.
const QUIET: Duration = Duration::from_millis(500);

/// What a device's feed said of thread `tid` from its session's
/// `voice.bound` on: how many `voice.ended`, `turn.done` and
/// `thread.deleted` named it, and whether a fresh `state` came.
#[derive(Debug, PartialEq)]
struct Heard {
    voice_ended: usize,
    turn_done: usize,
    thread_deleted: usize,
    state: bool,
}

/// Read both feeds past two later writes of the owner's — a live event the
/// session's end published is out by then — and say what the device's
/// heard of `tid` ([`Heard`]), and the reason the owner's `voice.ended`
/// gave.
async fn heard(w: &World, device: &mut Feed, owners: &mut Feed, tid: i64) -> (Heard, String) {
    for _ in 0..2 {
        let marker = chat_thread(w, &w.gw.client(), "chatty").await;
        let seen = |f: &[Frame]| {
            f.iter()
                .any(|f| f.event == "thread.created" && f.data["id"] == marker)
        };
        device.until(10, seen).await;
        owners.until(10, seen).await;
    }
    let names = |f: &Frame| f.data["thread_id"] == tid || f.data["id"] == tid;
    let from = device
        .frames
        .iter()
        .position(|f| f.event == "voice.bound" && names(f))
        .expect("the device heard its own bind");
    let after = &device.frames[from..];
    let count = |event: &str| {
        after
            .iter()
            .filter(|f| f.event == event && names(f))
            .count()
    };
    let heard = Heard {
        voice_ended: count("voice.ended"),
        turn_done: count("turn.done"),
        thread_deleted: count("thread.deleted"),
        state: after.iter().any(|f| f.event == "state"),
    };
    let reason = owners
        .frames
        .iter()
        .rev()
        .find(|f| f.event == "voice.ended" && f.data["thread_id"] == tid)
        .map(|f| f.data["reason"].as_str().unwrap_or("").to_string())
        .unwrap_or_default();
    (heard, reason)
}

/// The neutral close of thread `tid` (§1.6's close-code note).
fn out_of_reach(tid: i64) -> (u16, String) {
    (
        4004,
        format!("chat thread {tid} is out of reach for this key"),
    )
}

/// Run the write `start` begins while the store's write lock is held: the
/// session `ws` stays open as long as the write cannot commit, and closes
/// once it did. Its end. A close that comes early fails with what device
/// `d` then reads as its thread `old` and, given, its `folder`'s current
/// thread.
async fn closes_after_commit(
    w: &World,
    ws: &mut Ws,
    (d, old, folder): (&Device, i64, Option<i64>),
    start: impl FnOnce() -> tokio::task::JoinHandle<u16>,
) -> Ended {
    let held = hold_writes(w).await;
    let write = start();
    if let Some(early) = ended(ws, QUIET).await {
        let (by_id, _) = get(w, &d.client, &format!("/chat/api/threads/{old}")).await;
        let current = match folder {
            Some(f) => {
                let (s, v) = current(w, &d.client, f, false).await;
                format!("{s} {}", v["thread"]["id"])
            }
            None => "-".into(),
        };
        panic!(
            "the session closed before the write committed ({early:?}); thread {old} then read \
             {by_id} by id, and the folder's current thread {current}"
        );
    }
    held.rollback().await.unwrap();
    assert_eq!(write.await.unwrap(), 200, "the write lands");
    ended(ws, Duration::from_secs(10))
        .await
        .expect("the session closes once the write committed")
}

/// The device asks for the folder's current thread at once, as a client
/// does on the 4004 (`ws`'s): a new one, not `old`, and it binds and stays
/// bound.
async fn moved_on(
    w: &World,
    d: &Device,
    (ws, links): (&mut Ws, &mut Links),
    folder: i64,
    old: i64,
) {
    let (s, v) = current(w, &d.client, folder, false).await;
    assert_eq!(s, 200, "{v}");
    let new = v["thread"]["id"].as_i64().unwrap();
    assert_ne!(new, old, "current still names the thread out of reach: {v}");
    assert_eq!(v["rolled_over"], json!(true), "{v}");
    let (s, _) = get(w, &d.client, &format!("/chat/api/threads/{old}")).await;
    assert_eq!(s, 404, "by id the old thread is not there");
    settle(ws, links).await;
    let (mut again, _) = bind(w, d, new).await;
    assert_eq!(
        ended(&mut again, QUIET).await,
        None,
        "the thread current named binds and stays bound"
    );
}

/// The owner attaches the self-admin toolset to the thread a device is
/// bound to, the current thread of an ongoing folder: the 4004 comes once
/// the attach committed, and the folder's current thread has moved on by
/// then.
#[tokio::test]
async fn an_attach_closes_the_session_after_its_commit_and_current_has_moved_on() {
    let (w, d, _dir) = file_world(json!({})).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (old, _, _) = current_ok(&w, &d.client, folder, false).await;
    message(&w, old, 1).await;
    let (mut ws, mut links) = bind(&w, &d, old).await;

    let end = closes_after_commit(&w, &mut ws, (&d, old, Some(folder)), || {
        post_later(
            &w,
            &owner,
            &format!("/chat/api/threads/{old}/settings"),
            json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
        )
    })
    .await;
    assert_eq!((end.code, end.reason.clone()), out_of_reach(old));
    moved_on(&w, &d, (&mut ws, &mut links), folder, old).await;
}

/// The same through the folder's defaults, which reach its current thread
/// (L9).
#[tokio::test]
async fn an_attach_through_the_folder_s_defaults_closes_the_session_after_its_commit() {
    let (w, d, _dir) = file_world(json!({})).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (old, _, _) = current_ok(&w, &d.client, folder, false).await;
    let (mut ws, _links) = bind(&w, &d, old).await;

    let end = closes_after_commit(&w, &mut ws, (&d, old, Some(folder)), || {
        post_later(
            &w,
            &owner,
            &format!("/chat/api/folders/{folder}"),
            json!({ "defaults_patch": { "mcp_tools": [{ "server_label": "lmgw" }] } }),
        )
    })
    .await;
    assert_eq!((end.code, end.reason.clone()), out_of_reach(old));
    // The folder carries the toolset now: it is not there for the device.
    let (s, _) = get(&w, &d.client, &format!("/chat/api/threads/{old}")).await;
    assert_eq!(s, 404);
    let (s, _) = current(&w, &d.client, folder, false).await;
    assert_eq!(s, 404);
}

/// A device allowed lmgw's admin tools, bound to a toolset thread that is
/// its folder's current thread, has its level set to off: the 4004 comes
/// once the level is committed and published, and the folder's current
/// thread has moved on by then.
#[tokio::test]
async fn a_level_drop_closes_the_session_after_its_commit_and_current_has_moved_on() {
    let (w, d, _dir) = file_world(json!({ "self_admin": "read_only" })).await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (old, _, _) = current_ok(&w, &d.client, folder, false).await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{old}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (kept, rolled, _) = current_ok(&w, &d.client, folder, false).await;
    assert_eq!((kept, rolled), (old, false), "the device reaches it");
    let (mut ws, mut links) = bind(&w, &d, old).await;

    let end = closes_after_commit(&w, &mut ws, (&d, old, Some(folder)), || {
        post_later(
            &w,
            &owner,
            "/api/op/key_set",
            json!({ "id": d.id, "self_admin": "off" }),
        )
    })
    .await;
    assert_eq!((end.code, end.reason.clone()), out_of_reach(old));
    moved_on(&w, &d, (&mut ws, &mut links), folder, old).await;
}

/// A device's session on a thread that is deleted ends as one whose thread
/// left the device's reach: the same `error` event, then the same neutral
/// 4004 (review W4-18) — whoever deleted it, with its folder, or by the
/// sweep's purge — and only once the delete committed. Its feed hears the
/// same as for a hidden thread too: `thread.deleted` and a fresh `state`,
/// no `voice.ended`; the owner's still says `thread_gone`. The owner's
/// session on a deleted thread is left alone, as it always was.
#[tokio::test]
async fn a_deleted_thread_ends_a_device_s_session_as_one_out_of_its_reach() {
    let (w, d, _dir) = file_world(json!({})).await;
    let owner = w.gw.client();
    let mut feed = Feed::open(&w, &d.client, "", None).await;
    let mut owners = Feed::open(&w, &owner, "", None).await;

    // The reference: a thread hidden from the device by an attach.
    let hidden = chat_thread(&w, &d.client, "chatty").await;
    let (mut ws, mut links) = bind(&w, &d, hidden).await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{hidden}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let reach_left = ended(&mut ws, Duration::from_secs(10))
        .await
        .expect("the attach closes the session")
        .of_any_thread(hidden);
    settle(&mut ws, &mut links).await;
    assert_eq!(
        reach_left.said,
        [(
            "error".to_string(),
            "chat_thread_not_found".to_string(),
            "chat thread N is out of reach for this key, and this session closes".to_string()
        )],
        "{reach_left:?}"
    );
    assert_eq!(
        (reach_left.code, reach_left.reason.as_str()),
        (4004, "chat thread N is out of reach for this key")
    );
    let (hide_heard, why) = heard(&w, &mut feed, &mut owners, hidden).await;
    assert_eq!(
        hide_heard,
        Heard {
            voice_ended: 0,
            turn_done: 0,
            thread_deleted: 1,
            state: true
        },
        "{:#?}",
        feed.frames
    );
    assert_eq!(why, "revoked", "the owner hears the device's reach");

    for how in ["by the owner", "by the device", "with its folder"] {
        let tid = chat_thread(&w, &d.client, "chatty").await;
        let (path, body) = match how {
            "with its folder" => {
                let (_, f) = post(&w, &owner, "/chat/api/folders", json!({ "name": "Gone" })).await;
                let folder = f["id"].as_i64().unwrap();
                let (s, _) = post(
                    &w,
                    &d.client,
                    &format!("/chat/api/threads/{tid}/move"),
                    json!({ "folder_id": folder }),
                )
                .await;
                assert_eq!(s, 200);
                (
                    format!("/chat/api/folders/{folder}/delete"),
                    json!({ "threads": "delete" }),
                )
            }
            _ => (format!("/chat/api/threads/{tid}/delete"), json!({})),
        };
        let client = if how == "by the device" {
            &d.client
        } else {
            &owner
        };
        let (mut ws, mut links) = bind(&w, &d, tid).await;
        let end = closes_after_commit(&w, &mut ws, (&d, tid, None), || {
            post_later(&w, client, &path, body)
        })
        .await;
        assert_eq!(end.of_any_thread(tid), reach_left, "deleted {how}");
        settle(&mut ws, &mut links).await;
        let (said, why) = heard(&w, &mut feed, &mut owners, tid).await;
        assert_eq!(said, hide_heard, "deleted {how}: {:#?}", feed.frames);
        assert_eq!(
            why, "thread_gone",
            "deleted {how}: the owner's feed says why"
        );
    }

    // The sweep's purge: a thread archived past the purge days.
    settings(&w.state, |s| s.chat_purge_days = 1).await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let (mut ws, mut links) = bind(&w, &d, tid).await;
    sqlx::query("UPDATE chat_threads SET archived_at = datetime('now', '-3 days') WHERE id = ?1")
        .bind(tid)
        .execute(&w.state.db)
        .await
        .unwrap();
    let end = closes_after_commit(&w, &mut ws, (&d, tid, None), || {
        let state = w.state.clone();
        tokio::spawn(async move {
            lmgw_core::server::chat_upkeep(&state).await;
            200
        })
    })
    .await;
    assert_eq!(end.of_any_thread(tid), reach_left, "purged by the sweep");
    settle(&mut ws, &mut links).await;
    let (said, why) = heard(&w, &mut feed, &mut owners, tid).await;
    assert_eq!(said, hide_heard, "purged: {:#?}", feed.frames);
    assert_eq!(why, "thread_gone", "purged: the owner's feed says why");
    let (s, _) = get(&w, &d.client, &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(s, 404, "purged");

    // The owner's session is left alone, as the delete always left it.
    let tid = chat_thread(&w, &owner, "chatty").await;
    let cookie = w.cookie();
    let mut mine = bind_with(&w, ("cookie", cookie.as_str()), tid).await;
    let (s, _) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(ended(&mut mine, QUIET).await, None);
}

/// A spoken turn in flight when its thread goes, before the session's close
/// comes (the thread changed in the store, no route ran): a device's turn
/// is refused alike for a deleted thread and a hidden one, in the close's
/// neutral words; the owner's turn on a deleted thread hears that it is
/// gone.
#[tokio::test]
async fn a_turn_in_flight_hears_the_same_for_a_deleted_thread_as_for_a_hidden_one() {
    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let bearer = format!("Bearer {}", d.key);
    let cookie = w.cookie();
    let mut said = Vec::new();
    for (how, auth) in [
        ("hidden", ("authorization", bearer.as_str())),
        ("deleted", ("authorization", bearer.as_str())),
        ("deleted", ("cookie", cookie.as_str())),
    ] {
        let tid = chat_thread(&w, &w.gw.client(), "chatty").await;
        let mut ws = bind_with(&w, auth, tid).await;
        send(
            &mut ws,
            json!({"type": "session.update", "session": {"type": "realtime",
                "output_modalities": ["text"], "audio": {"input": {"turn_detection": null}}}}),
        )
        .await;
        assert_eq!(next(&mut ws).await["type"], "session.updated");
        match how {
            "hidden" => {
                sqlx::query(
                    "UPDATE chat_threads SET mcp_tools = '[{\"server_label\":\"lmgw\"}]' \
                     WHERE id = ?1",
                )
                .bind(tid)
                .execute(&w.state.db)
                .await
                .unwrap();
            }
            _ => lmgw_core::store::delete_chat_thread(&w.state.db, tid, None)
                .await
                .unwrap(),
        }
        w.asr.push(Asr::Text("Hallo?"));
        say(&mut ws).await;
        let events = until_type(&mut ws, "response.done").await;
        let errors: Vec<(String, String)> = of_type(&events, "error")
            .iter()
            .map(|e| {
                (
                    e["error"]["code"].as_str().unwrap_or("").to_string(),
                    e["error"]["message"]
                        .as_str()
                        .unwrap_or("")
                        .replace(&format!("chat thread {tid} "), "chat thread N "),
                )
            })
            .collect();
        let done = events.last().unwrap();
        assert_eq!(done["response"]["status"], "failed", "{how}: {done}");
        said.push(errors);
    }
    let neutral = vec![(
        "chat_thread_not_found".to_string(),
        "chat thread N is out of reach for this key".to_string(),
    )];
    assert_eq!(said[0], neutral, "hidden");
    assert_eq!(said[1], neutral, "deleted, for the device");
    assert_eq!(said[2].len(), 1, "{:?}", said[2]);
    assert_eq!(said[2][0].0, "chat_thread_not_found");
    assert!(
        said[2][0].1.contains("is gone (deleted"),
        "the owner hears it is gone: {:?}",
        said[2]
    );
}

/// A device's voice turn in flight — its reply streaming — when the owner
/// attaches the toolset to its thread, or deletes it, while the write waits
/// for its commit: the device hears nothing of either before the commit,
/// and the turn's model call is not stopped (a delete used to cancel the
/// turn before its commit: the branch review's R-1). Once it committed,
/// the session ends with the close's neutral `error`, then the 4004, and
/// nothing it said names a delete.
#[tokio::test]
async fn a_turn_in_flight_hears_nothing_before_the_commit_of_a_delete_or_an_attach() {
    let (w, d, _dir) = file_world(json!({})).await;
    let owner = w.gw.client();
    for how in ["attach", "delete"] {
        let tid = chat_thread(&w, &d.client, "chatty").await;
        let (mut ws, mut links) = bind(&w, &d, tid).await;
        send(
            &mut ws,
            json!({"type": "session.update", "session": {"type": "realtime",
                "output_modalities": ["text"], "audio": {"input": {"turn_detection": null}}}}),
        )
        .await;
        assert_eq!(next(&mut ws).await["type"], "session.updated");
        let gate = held_reply(&w);
        w.asr.push(Asr::Text("Hallo?"));
        say(&mut ws).await;
        until(&mut ws, |e| e["type"] == "response.output_text.delta").await;
        let stopped = w.chat.seen.closed_early.load(Ordering::SeqCst);

        let held = hold_writes(&w).await;
        let write = match how {
            "attach" => post_later(
                &w,
                &owner,
                &format!("/chat/api/threads/{tid}/settings"),
                json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
            ),
            _ => post_later(
                &w,
                &owner,
                &format!("/chat/api/threads/{tid}/delete"),
                json!({}),
            ),
        };
        let early = quiet(&mut ws, QUIET).await;
        assert!(
            early.is_empty(),
            "{how}: before the commit the device heard {early:#?}"
        );
        assert_eq!(
            w.chat.seen.closed_early.load(Ordering::SeqCst),
            stopped,
            "{how}: the turn's model call was stopped before the commit"
        );
        held.rollback().await.unwrap();
        assert_eq!(write.await.unwrap(), 200, "{how}");
        let end = ended(&mut ws, Duration::from_secs(10))
            .await
            .unwrap_or_else(|| panic!("{how}: the session closes once the write committed"));
        gate.notify_one();
        assert_eq!((end.code, end.reason.clone()), out_of_reach(tid), "{how}");
        let end = end.of_any_thread(tid);
        assert_eq!(
            end.said.last(),
            Some(&(
                "error".to_string(),
                "chat_thread_not_found".to_string(),
                "chat thread N is out of reach for this key, and this session closes".to_string()
            )),
            "{how}: {end:?}"
        );
        assert!(
            end.said
                .iter()
                .all(|(_, _, m)| !m.contains("delete") && !m.contains("gone")),
            "{how}: {end:?}"
        );
        settle(&mut ws, &mut links).await;
    }
}

/// The next reply streams its first word, then waits for the gate.
fn held_reply(w: &World) -> std::sync::Arc<tokio::sync::Notify> {
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Eins "),
        Step::Wait(gate.clone()),
        Step::Text("zwei."),
        Step::Finish("stop"),
        Step::Usage(12, 8),
    ]));
    gate
}

/// What an SSE response sends within `within`, as text.
async fn sse_within(resp: &mut reqwest::Response, within: Duration) -> String {
    let mut got = String::new();
    let _ = tokio::time::timeout(within, async {
        while let Ok(Some(c)) = resp.chunk().await {
            got.push_str(&String::from_utf8_lossy(&c));
        }
    })
    .await;
    got
}

/// The same for a device's text turn in flight (its `send` stream): the
/// turn is not cancelled before the delete's or the attach's commit, so
/// its stream says nothing of its end until then — a delete used to cancel
/// it before its commit (the branch review's R-1).
#[tokio::test]
async fn a_text_turn_in_flight_hears_nothing_before_the_commit_of_a_delete_or_an_attach() {
    let (w, d, _dir) = file_world(json!({})).await;
    let owner = w.gw.client();
    for how in ["attach", "delete"] {
        let tid = chat_thread(&w, &d.client, "chatty").await;
        let gate = held_reply(&w);
        let mut resp = d
            .client
            .post(format!("{}/chat/api/threads/{tid}/send", w.gw))
            .json(&json!({ "content": "Hallo?" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{how}");
        // Its first word came: the reply streams, then waits.
        let mut so_far = String::new();
        let first = tokio::time::timeout(Duration::from_secs(10), async {
            while !so_far.contains("Eins") {
                match resp.chunk().await {
                    Ok(Some(c)) => so_far.push_str(&String::from_utf8_lossy(&c)),
                    _ => break,
                }
            }
        })
        .await;
        assert!(first.is_ok() && so_far.contains("Eins"), "{how}: {so_far}");
        let stopped = w.chat.seen.closed_early.load(Ordering::SeqCst);

        let held = hold_writes(&w).await;
        let write = match how {
            "attach" => post_later(
                &w,
                &owner,
                &format!("/chat/api/threads/{tid}/settings"),
                json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
            ),
            _ => post_later(
                &w,
                &owner,
                &format!("/chat/api/threads/{tid}/delete"),
                json!({}),
            ),
        };
        let early = sse_within(&mut resp, QUIET).await;
        assert!(
            !early.contains("event: error") && !early.contains("event: done"),
            "{how}: before the commit the turn's stream said {early}"
        );
        assert_eq!(
            w.chat.seen.closed_early.load(Ordering::SeqCst),
            stopped,
            "{how}: the turn's model call was stopped before the commit"
        );
        held.rollback().await.unwrap();
        assert_eq!(write.await.unwrap(), 200, "{how}");
        let rest = sse_within(&mut resp, Duration::from_secs(10)).await;
        gate.notify_one();
        assert!(rest.contains("event: done"), "{how}: the turn ends: {rest}");
    }
}

/// The feed's side of a level drop: a device's stream that reads the
/// level's record between its commit and the publish of the snapshot that
/// says it holds the record until then, so the device hears its threads go
/// only once every route it reads says so — and nothing behind the record
/// is lost.
#[tokio::test]
async fn a_level_drop_reaches_a_device_s_feed_only_once_its_routes_say_it() {
    let w = crate::realtime_chat_thread::world(|_| {}).await;
    let owner = w.gw.client();
    let d = pair(&w, "phone", json!({ "self_admin": "read_only" })).await;
    let tools = super::self_admin_thread(&w).await;
    let mut feed = Feed::open(&w, &d.client, "", None).await;

    // The level's write as `key_set` commits it, and not yet its publish.
    let key = w
        .state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.id == d.id)
        .cloned()
        .unwrap();
    lmgw_core::store::update_key_policy(
        &w.state.db,
        d.id,
        &key.policy,
        key.enabled,
        &key.note,
        key.hosts_label.as_deref(),
        (DeviceAdmin::Off, Some(key.self_admin)),
    )
    .await
    .unwrap();
    // Another write wakes the feed meanwhile.
    let marker = chat_thread(&w, &owner, "chatty").await;
    let gone = |f: &[Frame]| {
        f.iter()
            .any(|f| f.event == "thread.deleted" && f.data["thread_id"] == tools.id)
    };
    let heard = tokio::time::timeout(Duration::from_secs(1), feed.until(10, gone))
        .await
        .is_ok();
    let (s, _) = get(&w, &d.client, &format!("/chat/api/threads/{}", tools.id)).await;
    assert_eq!(
        s, 200,
        "the device still reads it: the snapshot is not published"
    );
    assert!(
        !heard,
        "the feed said the thread went while the device still read it: {:#?}",
        feed.frames
    );
    // The marker's record, behind the level's, waited with it: the stream
    // read the level record and held it (no vacuous pass).
    assert!(
        !feed
            .frames
            .iter()
            .any(|f| f.event == "thread.created" && f.data["id"] == marker),
        "{:#?}",
        feed.frames
    );

    // Published, and the next wake plays it: the routes say it by then.
    w.state.reload_snapshot().await.unwrap();
    let after = chat_thread(&w, &owner, "chatty").await;
    feed.until(10, gone).await;
    let (s, _) = get(&w, &d.client, &format!("/chat/api/threads/{}", tools.id)).await;
    assert_eq!(s, 404);
    feed.until(10, |f| {
        f.iter()
            .any(|f| f.event == "thread.created" && f.data["id"] == after)
    })
    .await;
    let created: Vec<i64> = feed
        .named("thread.created")
        .iter()
        .filter_map(|f| f.data["id"].as_i64())
        .collect();
    assert_eq!(
        created,
        [marker, after],
        "nothing behind the record is lost"
    );
}
