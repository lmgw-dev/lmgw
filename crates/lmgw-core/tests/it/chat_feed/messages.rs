//! `message.*` (client-apps design §2.2, WP10; device link D18): every
//! message write is a stored event, rendered at delivery — `message.added`
//! and `message.updated` carry the messages whole, as
//! `GET /chat/api/threads/{id}` lists them, with the thread's title;
//! `message.deleted` the ids that went — in commit order, in the catch-up,
//! never about a thread a device may not see, and as the API document
//! describes them.

use serde_json::{json, Value};

use super::{about_thread, documented, Feed, Frame};
use crate::device_chat::{chat_thread, get, pair, post, sse, Hidden};
use crate::realtime_chat_thread::{world, World};

/// The messages of thread `tid` as `GET /chat/api/threads/{id}` lists them
/// now, by id.
async fn listed(w: &World, tid: i64) -> std::collections::BTreeMap<i64, Value> {
    let (status, v) = get(w, &w.gw.client(), &format!("/chat/api/threads/{tid}")).await;
    assert_eq!(status, 200, "{v}");
    v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["id"].as_i64().unwrap(), m.clone()))
        .collect()
}

/// The stored `message.*` frames of `frames`.
fn message_events(frames: &[Frame]) -> Vec<&Frame> {
    frames
        .iter()
        .filter(|f| f.id.is_some() && f.event.starts_with("message."))
        .collect()
}

/// The `message.*` frames of `frames` about thread `id`. (A thread that
/// took the self-admin toolset after it was made was a device's before:
/// its `thread.created` and `thread.deleted` are no message.)
fn hidden_messages(frames: &[Frame], id: i64) -> Vec<&Frame> {
    about_thread(frames, id)
        .into_iter()
        .filter(|f| f.event.starts_with("message."))
        .collect()
}

/// The ids a `message.*` frame names, in order.
fn ids_of(f: &Frame) -> Vec<i64> {
    match f.event.as_str() {
        "message.deleted" => f.data["message_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect(),
        _ => f.data["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_i64().unwrap())
            .collect(),
    }
}

/// A device's typed turn reaches the owner's feed as its user message and
/// its saved reply, each whole and by the device; a catch-up from before it
/// carries the same, as the thread lists them, with the title the first
/// send gave the thread.
#[tokio::test]
async fn a_turn_s_messages_arrive_whole_with_the_thread_s_title() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let mut owner = Feed::open(&w, &w.gw.client(), "", None).await;
    let start = owner.cursor();

    let (status, said) = sse(
        &w,
        &d.client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "hello there" }),
    )
    .await;
    assert_eq!(status, 200);
    let user = said.iter().find(|(e, _)| e == "turn").unwrap().1["user_message_id"]
        .as_i64()
        .unwrap();
    let reply = said.iter().find(|(e, _)| e == "done").unwrap().1["message_id"]
        .as_i64()
        .unwrap();

    owner
        .until(10, |f| {
            message_events(f)
                .iter()
                .filter(|f| f.event == "message.added")
                .count()
                >= 2
        })
        .await;
    let added: Vec<(Vec<i64>, Value)> = message_events(&owner.frames)
        .into_iter()
        .filter(|f| f.event == "message.added")
        .map(|f| (ids_of(f), f.data["by"].clone()))
        .collect();
    assert_eq!(
        added,
        vec![
            (vec![user], json!("device 'phone'")),
            (vec![reply], json!("device 'phone'")),
        ]
    );
    let saved = message_events(&owner.frames)
        .into_iter()
        .find(|f| ids_of(f) == vec![reply])
        .unwrap();
    assert_eq!(saved.data["thread_id"], tid);
    assert_eq!(saved.data["thread_title"], "hello there", "{}", saved.data);
    assert_eq!(saved.data["messages"][0]["role"], "assistant");
    documented("message.added", &saved.data);

    // Caught up from before the turn: rendered now, each message exactly as
    // the thread lists it.
    let mut caught = Feed::open(&w, &d.client, &format!("?since={start}"), None).await;
    caught
        .until(10, |f| {
            message_events(f)
                .iter()
                .filter(|f| f.event == "message.added")
                .count()
                >= 2
        })
        .await;
    let now = listed(&w, tid).await;
    for f in message_events(&caught.frames) {
        assert_eq!(f.data["thread_title"], "hello there");
        for m in f.data["messages"].as_array().unwrap() {
            let id = m["id"].as_i64().unwrap();
            assert_eq!(m, &now[&id], "message {id} as the thread lists it");
        }
    }
    assert!(caught.named("resync").is_empty());
}

/// An edit is `message.updated`, a delete `message.deleted`, and a
/// regenerate's cut the ids it took; a catch-up renders each as things are
/// now: a message gone by then is left out, and an event left with none is
/// not sent.
#[tokio::test]
async fn edits_and_deletes_arrive_and_a_catch_up_renders_them_now() {
    let w = world(|_| {}).await;
    let owner_client = w.gw.client();
    let tid = chat_thread(&w, &owner_client, "chatty").await;
    let mut live = Feed::open(&w, &owner_client, "", None).await;
    let start = live.cursor();

    let (status, said) = sse(
        &w,
        &owner_client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "first question" }),
    )
    .await;
    assert_eq!(status, 200);
    let user = said.iter().find(|(e, _)| e == "turn").unwrap().1["user_message_id"]
        .as_i64()
        .unwrap();
    let reply = said.iter().find(|(e, _)| e == "done").unwrap().1["message_id"]
        .as_i64()
        .unwrap();

    // The reply edited by hand.
    let (status, v) = post(
        &w,
        &owner_client,
        &format!("/chat/api/threads/{tid}/messages/{reply}/edit"),
        json!({ "content": "An edited answer." }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    // Regenerated: the reply goes, a new one comes.
    let (status, said) = sse(
        &w,
        &owner_client,
        &format!("/chat/api/threads/{tid}/messages/{reply}/regenerate"),
        json!({}),
    )
    .await;
    assert_eq!(status, 200);
    let again = said.iter().find(|(e, _)| e == "done").unwrap().1["message_id"]
        .as_i64()
        .unwrap();
    // The new reply deleted by itself.
    let (status, v) = post(
        &w,
        &owner_client,
        &format!("/chat/api/threads/{tid}/messages/{again}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{v}");

    let want = [
        ("message.added".to_string(), vec![user]),
        ("message.added".to_string(), vec![reply]),
        ("message.updated".to_string(), vec![reply]),
        ("message.deleted".to_string(), vec![reply]),
        ("message.added".to_string(), vec![again]),
        ("message.deleted".to_string(), vec![again]),
    ];
    // The last delete always renders: it reads nothing.
    live.until(10, |f| {
        message_events(f)
            .iter()
            .any(|f| f.event == "message.deleted" && ids_of(f) == vec![again])
    })
    .await;
    let got: Vec<(String, Vec<i64>)> = message_events(&live.frames)
        .into_iter()
        .map(|f| (f.event.clone(), ids_of(f)))
        .collect();
    // Each reply's record may have rendered before or after a later write
    // removed it: what arrives live is the writes in order, a record left
    // out only when its messages were gone by delivery.
    let mut writes = want.iter();
    for event in &got {
        assert!(
            writes.any(|w| w == event),
            "{event:?} is out of order in {got:?}"
        );
    }
    let edited = message_events(&live.frames)
        .into_iter()
        .find(|f| f.event == "message.updated");
    if let Some(edited) = edited {
        assert_eq!(edited.data["by"], "the dashboard");
        documented("message.updated", &edited.data);
    }
    let deleted = message_events(&live.frames)
        .into_iter()
        .rfind(|f| f.event == "message.deleted")
        .unwrap();
    assert_eq!(
        deleted.data,
        json!({ "thread_id": tid, "message_ids": [again], "by": "the dashboard" })
    );
    documented("message.deleted", &deleted.data);

    // Caught up from the start: both replies are gone, so their added and
    // updated records are not sent; the deletes are.
    let mut caught = Feed::open(&w, &owner_client, &format!("?since={start}"), None).await;
    caught
        .until(10, |f| {
            f.iter().filter(|f| f.event == "message.deleted").count() >= 2
        })
        .await;
    let got: Vec<(String, Vec<i64>)> = message_events(&caught.frames)
        .into_iter()
        .map(|f| (f.event.clone(), ids_of(f)))
        .collect();
    assert_eq!(
        got,
        vec![
            ("message.added".to_string(), vec![user]),
            ("message.deleted".to_string(), vec![reply]),
            ("message.deleted".to_string(), vec![again]),
        ]
    );
}

/// L3: a device's feed carries no message of a thread it does not see —
/// Admin Chat, or the self-admin toolset's — live or caught up, while the
/// owner's carries them.
#[tokio::test]
async fn a_device_never_hears_of_a_hidden_thread_s_messages() {
    for hidden in Hidden::BOTH {
        // The fixtures write through the store, which wakes no feed: the
        // keep-alive's read finds their records within a second.
        let w = world(|s| s.chat_feed_keepalive_s = 1).await;
        let d = pair(&w, "phone", json!({})).await;
        let mut device = Feed::open(&w, &d.client, "", None).await;
        let start = device.cursor();
        let mut owner = Feed::open(&w, &w.gw.client(), "", None).await;
        let thread = hidden.make(&w).await;
        // A message the device may see, after it: what the device's feed
        // must reach before the check means anything.
        let visible = chat_thread(&w, &d.client, "chatty").await;
        crate::device_chat::seed(&w, visible, "for the phone").await;

        owner
            .until(10, |f| {
                message_events(f)
                    .iter()
                    .any(|f| f.data["thread_id"] == thread.id)
            })
            .await;
        device
            .until(10, |f| {
                message_events(f)
                    .iter()
                    .any(|f| f.data["thread_id"] == visible)
            })
            .await;
        assert!(
            hidden_messages(&device.frames, thread.id).is_empty(),
            "{hidden:?}: {:#?}",
            device.frames
        );
        let mut caught = Feed::open(&w, &d.client, &format!("?since={start}"), None).await;
        caught
            .until(10, |f| {
                message_events(f)
                    .iter()
                    .any(|f| f.data["thread_id"] == visible)
            })
            .await;
        assert!(
            hidden_messages(&caught.frames, thread.id).is_empty(),
            "{hidden:?} in the catch-up: {:#?}",
            caught.frames
        );
    }
}

/// Delivery reads the thread's reach as it is now (L3, review of WP10): a
/// message written and one deleted while the thread was a device's, and
/// the thread then given the self-admin toolset, reach the device's
/// catch-up as neither `message.added` nor `message.deleted` — the toolset's
/// own `thread.deleted` says the thread went.
#[tokio::test]
async fn a_catch_up_leaves_out_the_messages_of_a_thread_hidden_since() {
    let w = world(|s| s.chat_feed_keepalive_s = 1).await;
    let d = pair(&w, "phone", json!({})).await;
    let owner_client = w.gw.client();
    let start = Feed::open(&w, &d.client, "", None).await.cursor();

    let tid = chat_thread(&w, &owner_client, "chatty").await;
    let (kept, _) = crate::device_chat::seed(&w, tid, "written while visible").await;
    let (gone, _) = crate::device_chat::seed(&w, tid, "deleted while visible").await;
    let (status, v) = post(
        &w,
        &owner_client,
        &format!("/chat/api/threads/{tid}/messages/{gone}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let (status, v) = post(
        &w,
        &owner_client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    // Something the device sees after it, so its catch-up is known to have
    // passed the hidden thread's records.
    let visible = chat_thread(&w, &d.client, "chatty").await;
    crate::device_chat::seed(&w, visible, "for the phone").await;
    let past = |f: &[Frame]| {
        message_events(f)
            .iter()
            .any(|f| f.data["thread_id"] == visible)
    };

    let mut caught = Feed::open(&w, &d.client, &format!("?since={start}"), None).await;
    caught.until(10, past).await;
    assert!(
        hidden_messages(&caught.frames, tid).is_empty(),
        "no message of a thread hidden since: {:#?}",
        caught.frames
    );
    // The owner's catch-up has them: the records are there, the reach is
    // the device's.
    let mut owner = Feed::open(&w, &owner_client, &format!("?since={start}"), None).await;
    owner.until(10, past).await;
    let theirs: Vec<(String, Vec<i64>)> = hidden_messages(&owner.frames, tid)
        .into_iter()
        .map(|f| (f.event.clone(), ids_of(f)))
        .collect();
    assert_eq!(
        theirs,
        vec![
            ("message.added".to_string(), vec![kept]),
            ("message.deleted".to_string(), vec![gone]),
        ],
        "the deleted message's own added record is not sent: it is gone"
    );
}

/// The positive side of L3: a device allowed lmgw's admin tools
/// (`read_only`, under the gateway's default `read_only`) hears the
/// messages of a thread with the self-admin toolset, live and caught up;
/// an Admin Chat thread's it still never does.
#[tokio::test]
async fn a_read_only_device_hears_the_toolset_s_messages() {
    let w = world(|s| s.chat_feed_keepalive_s = 1).await;
    let d = pair(&w, "tablet", json!({ "self_admin": "read_only" })).await;
    let mut live = Feed::open(&w, &d.client, "", None).await;
    let start = live.cursor();
    let toolset = Hidden::SelfAdmin.make(&w).await;
    let admin = Hidden::Admin.make(&w).await;
    let visible = chat_thread(&w, &d.client, "chatty").await;
    crate::device_chat::seed(&w, visible, "for the tablet").await;
    let past = |f: &[Frame]| {
        message_events(f)
            .iter()
            .any(|f| f.data["thread_id"] == visible)
    };

    live.until(10, past).await;
    let mut caught = Feed::open(&w, &d.client, &format!("?since={start}"), None).await;
    caught.until(10, past).await;
    for (feed, what) in [(&live, "live"), (&caught, "caught up")] {
        let heard: Vec<Vec<i64>> = hidden_messages(&feed.frames, toolset.id)
            .into_iter()
            .map(ids_of)
            .collect();
        assert_eq!(heard, vec![vec![toolset.message]], "{what}");
        assert!(
            hidden_messages(&feed.frames, admin.id).is_empty(),
            "{what}: never an Admin Chat thread's"
        );
    }
}
