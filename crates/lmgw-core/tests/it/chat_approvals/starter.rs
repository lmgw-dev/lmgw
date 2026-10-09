//! Who a resumed turn runs as, and who approved (client-apps design L13,
//! §6.3, §6.5).

use serde_json::json;

use super::{approval_frames, approved_by, attach, both_calls, decide, done, send};
use crate::chat_feed::Feed;
use crate::device_chat::{chat_thread, op, rows_of};
use crate::mcp_host::{host_world, linked};
use crate::support::realtime_fakes::Turn;

/// A device's gated turn, approved by the owner, resumes as the device:
/// its model call is checked against and charged to the device's key, and
/// the forwarded call runs as the device while naming the owner as its
/// approver — on the row, in the feed and in `_meta`.
#[tokio::test]
async fn the_resumed_turn_runs_as_its_starter_and_names_its_approver() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo", "notify"]).await;
    let owner = w.gw.client();
    let mut feed = Feed::open(&w, &owner, "", None).await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    attach(&w, &d.client, tid).await;
    both_calls(&w);
    let frames = send(&w, &d.client, tid, "tell me").await;
    let id = approval_frames(&frames)[0]["approval_request_id"]
        .as_str()
        .unwrap()
        .to_string();
    let reply = done(&frames)["message_id"].as_i64().unwrap();
    let chat_rows = |rows: &[(String, String, i64, Option<String>)]| {
        rows.iter().filter(|r| r.1 == "chatty").count()
    };
    let before = chat_rows(&rows_of(&w, d.id).await);

    w.chat.push(Turn::text(&["Done."]));
    let (s, frames) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": id, "approve": true}]),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    assert_eq!(done(&frames)["message_id"], reply);
    assert_eq!(
        chat_rows(&rows_of(&w, d.id).await),
        before + 1,
        "the resumed turn's model call is the device's"
    );
    let notify = loop {
        let c = dev.next_call().await;
        if c["params"]["name"] == "notify" {
            break c;
        }
    };
    let meta = &notify["params"]["_meta"];
    assert_eq!(
        meta["lmgw/caller"],
        json!({"kind": "device", "name": "desktop"}),
        "{notify}"
    );
    assert_eq!(
        meta["lmgw/approval"],
        json!({"decision": "approved", "by": {"kind": "owner", "name": "dashboard"}}),
        "{notify}"
    );
    assert_eq!(
        approved_by(&w, "desktop__notify").await,
        vec![Some("owner:dashboard".to_string())]
    );

    feed.until(10, |f| f.iter().any(|f| f.event == "approval.decided"))
        .await;
    let asked = feed.named("approval.requested");
    assert_eq!(asked.len(), 1, "{:?}", feed.frames);
    let a = &asked[0].data;
    assert_eq!(a["thread_id"], tid);
    assert_eq!(a["message_id"], reply);
    assert_eq!(a["approval_request_id"], id);
    assert_eq!(a["name"], "notify");
    assert_eq!(a["arguments"], r#"{"text":"hi"}"#);
    assert_eq!(
        a["call_id"], "call_2",
        "the call's id, as its frames carry it"
    );
    let decided = &feed.named("approval.decided")[0].data;
    assert_eq!(decided["approval_request_id"], id);
    assert_eq!(decided["call_id"], "call_2");
    assert_eq!(decided["approve"], true);
    assert_eq!(decided["by"], "the dashboard", "{decided}");
}

/// A starter whose key was disabled cannot run the resumed turn: the
/// decision is refused naming the key, and nothing is decided.
#[tokio::test]
async fn a_starter_whose_key_is_disabled_is_refused_by_name() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo", "notify"]).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &d.client, "chatty").await;
    attach(&w, &d.client, tid).await;
    both_calls(&w);
    let frames = send(&w, &d.client, tid, "tell me").await;
    let id = approval_frames(&frames)[0]["approval_request_id"].clone();
    let (s, v) = op(&w, "key_set", json!({ "id": d.id, "enabled": false })).await;
    assert_eq!(s, 200, "{v}");

    let (s, r) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": id, "approve": true}]),
    )
    .await;
    assert_eq!(s, 409, "{r:?}");
    assert_eq!(r[0].1["code"], "approval_starter_unavailable");
    assert!(
        r[0].1["message"].as_str().unwrap().contains("desktop"),
        "{r:?}"
    );
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    let last = v["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["pending_approvals"][0]["approval_request_id"], id,
        "still waiting: {v}"
    );
}
