//! What the Chat page reads and writes while it follows other writers
//! (client-apps design §3.6), beyond the frames themselves (`chat_events`):
//!
//! - `GET /chat/api/threads/rows?ids=` answers the named rows exactly as the
//!   list lists them, leaves out what is not there, and is the owner's
//!   alone (review CL-11);
//! - the server's side of a draft whose conversation was deleted elsewhere
//!   (review CL-14): the page's reads while it waits write nothing and leave
//!   an ongoing folder's current thread where the other client put it, and
//!   the folder's own "New conversation" takes the draft. That the page
//!   makes nothing until Send is pinned by `chat_page_gone_scan` and lmgw-ui
//!   (`pages/chat_sync/gone/flow.rs`), and driven by
//!   `scripts/drive/chat-live.json`.

use lmgw_core::store;
use serde_json::{json, Value};

use crate::chat_ongoing::{current_ok, ongoing_folder};
use crate::device_chat::{chat_thread, device_world, get, post, sse};
use crate::realtime_chat_thread::World;
use crate::support::realtime_fakes::Turn;

/// The ids of a list's `threads`, in order.
fn ids(list: &Value) -> Vec<i64> {
    list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect()
}

/// Every stored thread, active and archived, as the owner lists them.
async fn all(w: &World) -> Value {
    let (s, v) = get(w, &w.gw.client(), "/chat/api/threads?archived=all").await;
    assert_eq!(s, 200, "{v}");
    v
}

/// The current thread of ongoing folder `folder`, as the owner lists it.
async fn current_of(w: &World, folder: i64) -> Option<i64> {
    let (s, v) = get(w, &w.gw.client(), "/chat/api/folders").await;
    assert_eq!(s, 200, "{v}");
    v["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == folder)
        .unwrap_or_else(|| panic!("folder {folder} is not listed: {v}"))["ongoing"]
        ["current_thread_id"]
        .as_i64()
}

/// A turn of `client` in thread `tid`, answered with `reply`.
async fn turn(w: &World, client: &reqwest::Client, tid: i64, text: &str, reply: &'static str) {
    w.chat.push(Turn::text(&[reply]));
    let (s, frames) = sse(
        w,
        client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": text }),
    )
    .await;
    assert_eq!(s, 200);
    assert!(frames.iter().any(|(e, _)| e == "done"), "{frames:?}");
}

#[tokio::test]
async fn the_rows_route_answers_the_named_rows_as_the_list_lists_them() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let a = chat_thread(&w, &owner, "chatty").await;
    let b = chat_thread(&w, &owner, "chatty").await;
    let c = chat_thread(&w, &owner, "chatty").await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{b}/pin"),
        json!({ "pinned": true }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{c}/archive"),
        json!({ "archived": true }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    turn(&w, &owner, a, "hello", "Hi.").await;

    let list = all(&w).await;
    let (s, rows) = get(
        &w,
        &owner,
        &format!("/chat/api/threads/rows?ids={a},{c},{b},{a},999999,-4"),
    )
    .await;
    assert_eq!(s, 200, "{rows}");
    let named = [a, b, c];
    let in_list: Vec<i64> = ids(&list)
        .into_iter()
        .filter(|id| named.contains(id))
        .collect();
    assert_eq!(
        ids(&rows),
        in_list,
        "each named thread once, in the list's order; one that is not there, and a \
         temporary id, are left out"
    );
    for row in rows["threads"].as_array().unwrap() {
        let listed = list["threads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == row["id"])
            .unwrap();
        assert_eq!(row, listed, "a row is the list's row, field for field");
    }
    let row_a =
        &rows["threads"].as_array().unwrap()[ids(&rows).iter().position(|id| *id == a).unwrap()];
    assert!(row_a["last_message_at"].is_i64(), "{row_a}");

    let (s, v) = get(&w, &owner, "/chat/api/threads/rows?ids=").await;
    assert_eq!((s, v), (200, json!({ "threads": [] })));
    let (s, v) = get(&w, &owner, "/chat/api/threads/rows?ids=3,x").await;
    assert_eq!((s, v["code"].as_str()), (400, Some("bad_request")), "{v}");
    assert!(v["message"].as_str().unwrap().contains("'x'"), "{v}");

    // The owner's alone: a device's own feed carries rows.
    let (s, v) = get(&w, &d.client, &format!("/chat/api/threads/rows?ids={a}")).await;
    assert_eq!(s, 403, "{v}");
}

/// The server's side of the rescue, request by request: Kai deletes the
/// conversation the owner was writing into and goes on in a fresh one. The
/// reads the page makes while the draft waits (the list, the rows a frame
/// named, the deleted thread's 404) write nothing and leave Kai's current
/// thread where it is. The folder's own "New conversation", which Send
/// makes, rolls over to a new current thread that takes the draft and
/// leaves Kai's conversation alone. That the page sends no request before
/// Send is not checked here: `chat_page_gone_scan` pins it (review CF-7).
#[tokio::test]
async fn a_rescued_draft_s_reads_write_nothing_and_new_conversation_takes_the_draft() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let folder = ongoing_folder(&w, &d.client, "desktop", 0, json!({})).await;
    let (open, _, _) = current_ok(&w, &d.client, folder, false).await;
    turn(&w, &d.client, open, "hello", "Hi.").await;

    // Kai deletes the open conversation and goes on in a fresh current one.
    let (s, v) = post(
        &w,
        &d.client,
        &format!("/chat/api/threads/{open}/delete"),
        json!({}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (fresh, _, _) = current_ok(&w, &d.client, folder, false).await;
    assert_ne!(fresh, open);
    turn(&w, &d.client, fresh, "still there?", "Yes.").await;
    let before = ids(&all(&w).await);

    // The page's reads while the draft waits for Send.
    let (s, v) = get(&w, &owner, "/chat/api/threads").await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = get(&w, &owner, &format!("/chat/api/threads/rows?ids={fresh}")).await;
    assert_eq!(ids(&v), vec![fresh], "{s}: {v}");
    let (s, v) = get(&w, &owner, &format!("/chat/api/threads/{open}")).await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");
    assert_eq!(
        current_of(&w, folder).await,
        Some(fresh),
        "Kai's current thread is untouched until Send"
    );
    assert_eq!(ids(&all(&w).await), before, "nothing was made");

    // Send: the folder's own "New conversation", then the turn.
    let (made, rolled, _) = current_ok(&w, &owner, folder, true).await;
    assert!(rolled && made != fresh && !before.contains(&made));
    turn(&w, &owner, made, "my draft", "Noted.").await;
    assert_eq!(current_of(&w, folder).await, Some(made));
    let said = |tid: i64| {
        let w = &w;
        async move {
            store::list_chat_messages(&w.state.db, tid)
                .await
                .unwrap()
                .into_iter()
                .map(|m| m.content)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(said(made).await, vec!["my draft", "Noted."]);
    assert!(!said(fresh).await.contains(&"my draft".to_string()));
}
