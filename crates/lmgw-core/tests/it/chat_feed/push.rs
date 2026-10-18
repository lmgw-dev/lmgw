//! The feed as a device's push channel (device link design §7, WP5): a
//! stream's own keep-alive up to `chat_feed_keepalive_max_s`, the backstop
//! that keeps the gateway's interval whatever a stream asked,
//! `hello.open_approvals` and `caught_up`, read as `lmgw-client`'s
//! `FeedReader` combines them, and `approval.requested.thread_title`, each
//! as the document describes it.

use lmgw_client::feed::{ApprovalsChange, FeedItem, FeedReader};
use serde_json::{json, Value};

use super::{documented, set, Feed, Frame};
use crate::chat_approvals::{approval_frames, decide, send};
use crate::device_chat::{chat_thread, get, op, pair, post};
use crate::realtime_chat_thread::{world, World};
use crate::support::realtime_fakes::Turn;
use crate::support::realtime_mcp::calls;

/// `GET /chat/api/feed{query}` as the owner, expected to be refused: the
/// status and the body.
async fn refused(w: &World, query: &str) -> (u16, Value) {
    let resp =
        w.gw.client()
            .get(format!("{}/chat/api/feed{query}", w.gw))
            .send()
            .await
            .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// §7.1: `hello.keepalive_s` is what the stream asked for, from 1 to the
/// maximum, or the gateway's without a request; past the maximum, at 0 or
/// below is a 400 naming the bound and the setting; a moved maximum
/// applies to the next feed.
#[tokio::test]
async fn a_stream_keeps_the_keepalive_it_asks_for_up_to_the_maximum() {
    let w = world(|s| s.chat_feed_keepalive_max_s = 30).await;
    let owner = w.gw.client();
    let plain = Feed::open(&w, &owner, "", None).await;
    assert_eq!(
        plain.hello()["keepalive_s"],
        15,
        "none asked: the gateway's"
    );
    for asked in [1, 5, 30] {
        let f = Feed::open(&w, &owner, &format!("?keepalive_s={asked}"), None).await;
        assert_eq!(f.hello()["keepalive_s"], asked);
        documented("hello", f.hello());
    }
    let phone = pair(&w, "phone", json!({})).await;
    let f = Feed::open(&w, &phone.client, "?keepalive_s=20", None).await;
    assert_eq!(f.hello()["keepalive_s"], 20, "a device asks the same way");
    for bad in ["31", "0", "-4"] {
        let (status, v) = refused(&w, &format!("?keepalive_s={bad}")).await;
        assert_eq!(
            (status, &v["code"]),
            (400, &json!("bad_request")),
            "{bad}: {v}"
        );
        let message = v["message"].as_str().unwrap();
        assert!(
            message.contains("from 1 to 30 seconds")
                && message.contains("chat_feed_keepalive_max_s"),
            "{message}"
        );
    }
    let (status, v) = refused(&w, "?keepalive_s=soon").await;
    assert_eq!(status, 400, "not a number: {v}");
    set(&w, |s| s.chat_feed_keepalive_max_s = 120).await;
    let f = Feed::open(&w, &owner, "?keepalive_s=120", None).await;
    assert_eq!(
        f.hello()["keepalive_s"],
        120,
        "the next feed may ask for more"
    );
}

/// §7.1's split tick: a stream that asked for a long keep-alive sends no
/// comment meanwhile, and a record whose writer did not wake the feed (a
/// store write: the layer above wakes it) still arrives at the gateway's
/// own interval.
#[tokio::test]
async fn a_long_keepalive_holds_back_no_event() {
    let w = world(|s| s.chat_feed_keepalive_s = 1).await;
    let mut long = Feed::open(&w, &w.gw.client(), "?keepalive_s=600", None).await;
    let t = lmgw_core::store::create_chat_thread(&w.state.db, "chatty", "chat")
        .await
        .unwrap();
    long.until(10, |f| {
        f.iter()
            .any(|f| f.event == "thread.created" && f.data["id"] == t)
    })
    .await;
    assert!(long.named(":").is_empty(), "no comment: {:?}", long.frames);
    // A stream at the gateway's interval gets its comments.
    let mut short = Feed::open(&w, &w.gw.client(), "", None).await;
    short.until(10, |f| f.iter().any(|f| f.event == ":")).await;
}

/// Review of WP5, N2: a device feed waiting for its levels to be published
/// sends its comments at the keep-alive it asked for, not one and then
/// silence for the gateway's longer one.
#[tokio::test]
async fn a_waiting_feed_keeps_the_keepalive_it_asked_for() {
    let w = world(|s| s.chat_feed_keepalive_s = 4).await;
    let d = pair(&w, "phone", json!({"self_admin": "off"})).await;
    // The store says another level than the published snapshot: the feed
    // waits for a publish that does not come, one gateway keep-alive long.
    sqlx::query("UPDATE api_keys SET self_admin = 1 WHERE id = ?1")
        .bind(d.id)
        .execute(&w.state.db)
        .await
        .unwrap();
    let resp = d
        .client
        .get(format!("{}/chat/api/feed?keepalive_s=1", w.gw))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut feed = Feed::reading(resp);
    feed.until(10, |f| f.iter().any(|f| f.event == "hello"))
        .await;
    let before = feed
        .frames
        .iter()
        .take_while(|f| f.event != "hello")
        .count();
    assert!(
        before >= 3,
        "a comment at once and one a second while it waits: {:?}",
        feed.frames
    );
    assert_eq!(feed.frames[before].data["keepalive_s"], 1);
}

/// A new thread of the owner's with `lmgw__status` gated, and a turn that
/// stopped on it: the thread and the waiting call's id.
async fn waiting_thread(w: &World, owner: &reqwest::Client) -> (i64, Value) {
    let tid = chat_thread(w, owner, "chatty").await;
    let (s, v) = post(
        w,
        owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "lmgw", "require_approval": "always"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    w.chat
        .push(calls(&[(0, "call_1", "lmgw__status", "{}")], "tool_calls"));
    let frames = send(w, owner, tid, "how are you").await;
    (
        tid,
        approval_frames(&frames)[0]["approval_request_id"].clone(),
    )
}

/// Thread `tid`'s title now.
async fn title_of(w: &World, tid: i64) -> Value {
    let (s, v) = get(w, &w.gw.client(), &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(s, 200, "{v}");
    v["thread"]["title"].clone()
}

/// `frames` as the SSE text they came as, for a client's reader.
fn sse_text(frames: &[Frame]) -> String {
    frames
        .iter()
        .map(|f| {
            let id =
                f.id.as_ref()
                    .map(|i| format!("id: {i}\n"))
                    .unwrap_or_default();
            if f.event == ":" {
                format!(": keep-alive\n{id}\n")
            } else {
                format!("{id}event: {}\ndata: {}\n\n", f.event, f.data)
            }
        })
        .collect()
}

/// The open-approval changes a reader yields for `frames`.
fn changes(reader: &mut FeedReader, frames: &[Frame]) -> Vec<ApprovalsChange> {
    reader
        .push(sse_text(frames).as_bytes())
        .into_iter()
        .filter_map(|i| match i {
            FeedItem::Approvals(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// The ids of a set of open calls.
fn ids(set: &[lmgw_client::feed::ApprovalRequested]) -> Vec<String> {
    set.iter().map(|a| a.approval_request_id.clone()).collect()
}

/// §7.3, §7.4: `hello.open_approvals` is the waiting set at the head, each
/// call with its thread's title; `caught_up` comes right after `hello`
/// with nothing to catch up, and after the catch-up's last event
/// otherwise; a catch-up holding a request and its decision leaves a
/// reader's set as `hello` gave it, and a decision after `caught_up`
/// withdraws; a device that may not see the thread is told of no call, one
/// that may of the same set; every event as the document describes it.
#[tokio::test]
async fn hello_lists_the_open_approvals_and_a_catch_up_changes_none() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let start = Feed::open(&w, &owner, "", None).await.cursor();

    // One call asked for and decided, then one that waits.
    let (done_tid, done_id) = waiting_thread(&w, &owner).await;
    w.chat.push(Turn::text(&["All well."]));
    let (s, frames) = decide(
        &w,
        &owner,
        done_tid,
        json!([{"approval_request_id": done_id, "approve": true}]),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    let (tid, id) = waiting_thread(&w, &owner).await;
    let title = title_of(&w, tid).await;

    // From now: nothing to catch up.
    let mut now = Feed::open(&w, &owner, "", None).await;
    now.until(10, |f| f.iter().any(|f| f.event == "caught_up"))
        .await;
    assert_eq!(now.frames[1].event, "caught_up", "{:?}", now.frames);
    assert_eq!(now.frames[1].data["cursor"], now.hello()["cursor"]);
    let open = now.hello()["open_approvals"].clone();
    assert_eq!(open.as_array().map(Vec::len), Some(1), "{open}");
    assert_eq!(
        (&open[0]["approval_request_id"], &open[0]["thread_id"]),
        (&id, &json!(tid))
    );
    assert_eq!(open[0]["thread_title"], title);
    assert_eq!(open[0]["name"], "status");
    documented("hello", now.hello());
    documented("caught_up", &now.frames[1].data);

    // From before both: the catch-up holds the waiting call's request and
    // the other call's decision; that call's request is not sent, decided by
    // delivery (review of WP5, N3).
    let mut back = Feed::open(&w, &owner, &format!("?since={start}"), None).await;
    back.until(10, |f| f.iter().any(|f| f.event == "caught_up"))
        .await;
    assert_eq!(back.hello()["open_approvals"], open);
    let at = back
        .frames
        .iter()
        .position(|f| f.event == "caught_up")
        .unwrap();
    let requested: Vec<&Frame> = back.frames[..at]
        .iter()
        .filter(|f| f.event == "approval.requested")
        .collect();
    assert_eq!(requested.len(), 1, "{:#?}", back.frames);
    assert!(back.frames[..at]
        .iter()
        .any(|f| f.event == "approval.decided" && f.data["approval_request_id"] == done_id));
    let waiting = requested[0];
    assert_eq!(waiting.data["approval_request_id"], id);
    assert_eq!(waiting.data["thread_title"], title);
    documented("approval.requested", &waiting.data);
    // Every stored event of the catch-up came before it; it names the head.
    assert!(back.frames[at + 1..].iter().all(|f| f.id.is_none()));
    assert_eq!(back.frames[at].data["cursor"], now.hello()["cursor"]);

    // A client's reader: the catch-up changes nothing, `caught_up` yields
    // the set as `hello` gave it.
    let mut reader = FeedReader::new(Some(start.as_str()));
    let got = changes(&mut reader, &back.frames);
    let [ApprovalsChange::Open(set)] = got.as_slice() else {
        panic!("{got:?}")
    };
    assert_eq!(ids(set), vec![id.as_str().unwrap().to_string()]);

    // L3: a device that may not use lmgw's admin tools does not see these
    // threads, so no call waits for it; one that may sees the same set.
    let phone = pair(&w, "phone", json!({"self_admin": "off"})).await;
    let theirs = Feed::open(&w, &phone.client, "", None).await;
    assert_eq!(theirs.hello()["open_approvals"], json!([]));
    let tablet = pair(&w, "tablet", json!({"self_admin": "read_only"})).await;
    let tablets = Feed::open(&w, &tablet.client, "", None).await;
    assert_eq!(tablets.hello()["open_approvals"], open);

    // Decided now: after `caught_up`, the decision withdraws it.
    w.chat.push(Turn::text(&["Not then."]));
    let (s, frames) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": id, "approve": false}]),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    let seen = back.frames.len();
    back.until(10, |f| {
        f.iter()
            .any(|f| f.event == "approval.decided" && f.data["approval_request_id"] == id)
    })
    .await;
    let got = changes(&mut reader, &back.frames[seen..]);
    assert_eq!(
        got,
        vec![ApprovalsChange::Withdrawn(vec![id
            .as_str()
            .unwrap()
            .to_string()])]
    );
    assert!(reader.open_approvals().is_empty());
    let none = Feed::open(&w, &owner, "", None).await;
    assert_eq!(none.hello()["open_approvals"], json!([]));
}

/// L3 for `hello.open_approvals` (review of WP5, N4): a call waiting in an
/// Admin Chat thread is the owner's alone, a `full` device under a `full`
/// gateway included; a `read_only` device under a gateway at `off` sees
/// no call of the self-admin toolset's threads.
#[tokio::test]
async fn hello_s_open_set_follows_the_reader_s_reach() {
    let w = world(|s| s.self_admin = lmgw_core::config::SelfAdmin::Full).await;
    let owner = w.gw.client();
    let (_, toolset) = waiting_thread(&w, &owner).await;
    let (admin_tid, admin) = waiting_thread(&w, &owner).await;
    sqlx::query("UPDATE chat_threads SET kind = 'admin' WHERE id = ?1")
        .bind(admin_tid)
        .execute(&w.state.db)
        .await
        .unwrap();
    let open_ids = |f: &Feed| -> Vec<Value> {
        f.hello()["open_approvals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["approval_request_id"].clone())
            .collect()
    };
    let mine = Feed::open(&w, &owner, "", None).await;
    assert_eq!(open_ids(&mine), vec![toolset.clone(), admin.clone()]);
    let full = pair(&w, "desk", json!({"self_admin": "full"})).await;
    let theirs = Feed::open(&w, &full.client, "", None).await;
    assert_eq!(theirs.hello()["self_admin"], "full");
    assert_eq!(open_ids(&theirs), vec![toolset], "no Admin Chat call");

    set(&w, |s| s.self_admin = lmgw_core::config::SelfAdmin::Off).await;
    let reader = pair(&w, "tablet", json!({"self_admin": "read_only"})).await;
    let theirs = Feed::open(&w, &reader.client, "", None).await;
    assert_eq!(theirs.hello()["self_admin"], "off", "capped by the gateway");
    assert!(open_ids(&theirs).is_empty(), "{:?}", theirs.hello());
}

/// §7.3: `caught_up` follows the last event of a catch-up read in several
/// pages, once, and before any event written after the head.
#[tokio::test]
async fn caught_up_follows_a_paged_catch_up() {
    let w = world(|s| s.chat_feed_page_size = 2).await;
    let owner = w.gw.client();
    let start = Feed::open(&w, &owner, "", None).await.cursor();
    let mut made = Vec::new();
    for _ in 0..5 {
        made.push(chat_thread(&w, &owner, "chatty").await);
    }
    let mut back = Feed::open(&w, &owner, &format!("?since={start}"), None).await;
    back.until(10, |f| f.iter().any(|f| f.event == "caught_up"))
        .await;
    let at = back
        .frames
        .iter()
        .position(|f| f.event == "caught_up")
        .unwrap();
    let created: Vec<i64> = back.frames[..at]
        .iter()
        .filter(|f| f.event == "thread.created")
        .map(|f| f.data["id"].as_i64().unwrap())
        .collect();
    assert_eq!(created, made);
    let last = back.frames[..at]
        .iter()
        .rev()
        .find_map(|f| f.id.clone())
        .unwrap();
    assert_eq!(back.frames[at].data["cursor"], json!(last));
    documented("caught_up", &back.frames[at].data);

    let later = chat_thread(&w, &owner, "chatty").await;
    back.until(10, |f| {
        f.iter()
            .any(|f| f.event == "thread.created" && f.data["id"] == later)
    })
    .await;
    let after = back
        .frames
        .iter()
        .position(|f| f.event == "thread.created" && f.data["id"] == later)
        .unwrap();
    assert!(after > at);
    assert_eq!(back.named("caught_up").len(), 1, "once per stream");
}

/// Whether `frames` hold a `state` whose open set names call `id`.
fn state_naming(frames: &[Frame], id: &Value) -> bool {
    frames.iter().any(|f| {
        f.event == "state"
            && f.data["open_approvals"]
                .as_array()
                .is_some_and(|a| a.iter().any(|c| &c["approval_request_id"] == id))
    })
}

/// A device's feed after `caught_up`, with the reader that followed it.
async fn caught_up_device(w: &World, d: &crate::device_chat::Device) -> (Feed, FeedReader) {
    let mut feed = Feed::open(w, &d.client, "", None).await;
    feed.until(10, |f| f.iter().any(|f| f.event == "caught_up"))
        .await;
    let mut reader = FeedReader::new(None);
    let got = changes(&mut reader, &feed.frames);
    assert!(
        matches!(got.as_slice(), [ApprovalsChange::Open(_)]),
        "{got:?}"
    );
    (feed, reader)
}

/// The owner's ruling of 2026-10-15 (§7.3): a thread that comes into a
/// device's reach after `caught_up` while a call waits in it — its level
/// raised — brings that call in a `state`, which the reader yields as
/// `Added`; the reach lost again withdraws it.
#[tokio::test]
async fn a_level_raise_brings_the_calls_waiting_in_reach_and_a_loss_withdraws_them() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let (tid, id) = waiting_thread(&w, &owner).await;
    let phone = pair(&w, "phone", json!({"self_admin": "off"})).await;
    let (mut feed, mut reader) = caught_up_device(&w, &phone).await;
    assert_eq!(feed.hello()["open_approvals"], json!([]));
    assert!(reader.open_approvals().is_empty());

    let seen = feed.frames.len();
    let (s, v) = op(
        &w,
        "key_set",
        json!({"id": phone.id, "self_admin": "read_only"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    feed.until(10, |f| state_naming(f, &id)).await;
    let came = feed.frames[seen..]
        .iter()
        .position(|f| f.event == "thread.created" && f.data["id"] == tid)
        .expect("the thread arrives");
    let state = feed.frames[seen..]
        .iter()
        .position(|f| state_naming(std::slice::from_ref(f), &id))
        .unwrap();
    assert!(came < state, "{:#?}", &feed.frames[seen..]);
    documented("state", &feed.frames[seen + state].data);
    let got = changes(&mut reader, &feed.frames[seen..]);
    let [ApprovalsChange::Added(a)] = got.as_slice() else {
        panic!("{got:?}")
    };
    assert_eq!(json!(a.approval_request_id), id);

    let seen = feed.frames.len();
    let (s, v) = op(&w, "key_set", json!({"id": phone.id, "self_admin": "off"})).await;
    assert_eq!(s, 200, "{v}");
    feed.until(10, |f| {
        f.iter()
            .any(|f| f.event == "thread.deleted" && f.data["thread_id"] == tid)
    })
    .await;
    feed.until(10, |f| f[seen..].iter().any(|f| f.event == "state"))
        .await;
    let got = changes(&mut reader, &feed.frames[seen..]);
    assert_eq!(
        got,
        vec![ApprovalsChange::Withdrawn(vec![id
            .as_str()
            .unwrap()
            .to_string()])]
    );
    assert!(reader.open_approvals().is_empty());
}

/// The owner's ruling of 2026-10-15 (§7.3): taking the self-admin toolset
/// off a thread brings it into a device's reach (`thread.created`), and the
/// call still waiting in it in a `state`: `Added` for the reader.
#[tokio::test]
async fn taking_the_toolset_off_brings_the_waiting_call_to_a_device() {
    let w = world(|_| {}).await;
    let owner = w.gw.client();
    let (tid, id) = waiting_thread(&w, &owner).await;
    let phone = pair(&w, "phone", json!({"self_admin": "off"})).await;
    let (mut feed, mut reader) = caught_up_device(&w, &phone).await;
    let seen = feed.frames.len();
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": []}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    feed.until(10, |f| state_naming(f, &id)).await;
    assert!(feed.frames[seen..]
        .iter()
        .any(|f| f.event == "thread.created" && f.data["id"] == tid));
    let got = changes(&mut reader, &feed.frames[seen..]);
    let [ApprovalsChange::Added(a)] = got.as_slice() else {
        panic!("{got:?}")
    };
    assert_eq!(json!(a.approval_request_id), id);
    assert_eq!(a.thread_id, tid);
}
