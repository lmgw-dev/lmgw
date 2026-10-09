//! The `approval.*` records reach only a reader that sees the thread
//! (client-apps design §6.5, L3).

use serde_json::json;

use super::{approval_frames, decide, send};
use crate::chat_feed::Feed;
use crate::device_chat::{chat_thread, pair, post};
use crate::mcp_host::host_world;
use crate::support::realtime_fakes::Turn;
use crate::support::realtime_mcp::calls;

/// The owner's thread with lmgw's admin tools asks for and gets an
/// approval: the owner's feed carries both records, a device that may not
/// use the admin tools — the thread does not exist for it — neither,
/// while it still reads the owner's next plain thread.
#[tokio::test]
async fn approval_records_are_hidden_from_a_device_that_cannot_see_the_thread() {
    let (w, _d) = host_world().await;
    let phone = pair(&w, "phone", json!({ "self_admin": "off" })).await;
    let owner = w.gw.client();
    let mut mine = Feed::open(&w, &owner, "", None).await;
    let mut theirs = Feed::open(&w, &phone.client, "", None).await;

    let tid = chat_thread(&w, &owner, "chatty").await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "lmgw", "require_approval": "always"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    w.chat
        .push(calls(&[(0, "call_1", "lmgw__status", "{}")], "tool_calls"));
    let frames = send(&w, &owner, tid, "how are you").await;
    let id = approval_frames(&frames)[0]["approval_request_id"].clone();
    w.chat.push(Turn::text(&["All well."]));
    let (s, frames) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": id, "approve": true}]),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    mine.until(10, |f| f.iter().any(|f| f.event == "approval.decided"))
        .await;
    assert_eq!(
        mine.named("approval.requested").len(),
        1,
        "{:?}",
        mine.frames
    );

    // A plain thread afterwards: the device reads it, so it has read past
    // the approval records by then.
    let plain = chat_thread(&w, &owner, "chatty").await;
    theirs
        .until(10, |f| {
            f.iter()
                .any(|f| f.event == "thread.created" && f.data["id"] == plain)
        })
        .await;
    assert!(
        theirs
            .frames
            .iter()
            .all(|f| !f.event.starts_with("approval.")),
        "{:#?}",
        theirs.frames
    );
}
