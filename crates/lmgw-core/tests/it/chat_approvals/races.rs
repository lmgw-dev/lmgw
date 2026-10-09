//! Two deciders at once, and a temporary thread's approvals (client-apps
//! design §6.3, §6.6).

use serde_json::json;

use super::{approval_frames, approved_by, attach, both_calls, decide, done, send};
use crate::device_chat::{chat_thread, post};
use crate::mcp_host::{host_world, linked};
use crate::support::realtime_fakes::Turn;

/// The owner and a device decide the same call at once over HTTP: one
/// resumes the turn, the other is told who decided, and the call runs once.
#[tokio::test]
async fn of_two_deciders_racing_one_wins_and_the_call_runs_once() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo", "notify"]).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    attach(&w, &owner, tid).await;
    both_calls(&w);
    let frames = send(&w, &owner, tid, "tell me").await;
    let id = approval_frames(&frames)[0]["approval_request_id"].clone();

    w.chat.push(Turn::text(&["Done."]));
    let verdict = json!([{"approval_request_id": id, "approve": true}]);
    let (a, b) = tokio::join!(
        decide(&w, &owner, tid, verdict.clone()),
        decide(&w, &d.client, tid, verdict.clone()),
    );
    let mut statuses = [a.0, b.0];
    statuses.sort();
    assert_eq!(statuses, [200, 409], "{a:?} {b:?}");
    let (won, lost) = if a.0 == 200 { (a, b) } else { (b, a) };
    assert_eq!(lost.1[0].1["code"], "approval_decided", "{lost:?}");
    assert_eq!(done(&won.1)["aborted"], false, "{won:?}");
    let rows = approved_by(&w, "desktop__notify").await;
    assert_eq!(rows.len(), 1, "the call ran once: {rows:?}");
    let named = lost.1[0].1["message"].as_str().unwrap();
    let by = rows[0].as_deref().unwrap();
    // The loser is told the winner, who is the row's approver.
    if by == "owner:dashboard" {
        assert!(named.contains("the dashboard"), "{named}");
    } else {
        assert_eq!(by, "device:desktop");
        assert!(named.contains("desktop"), "{named}");
    }
    let mut names = Vec::new();
    for _ in 0..2 {
        names.push(dev.next_call().await["params"]["name"].clone());
    }
    names.sort_by_key(|n| n.to_string());
    assert_eq!(names, [json!("echo"), json!("notify")]);
}

/// A temporary thread takes approvals too, in memory: the turn stops, the
/// decision resumes it onto the same reply, and the approved call names
/// its approver.
#[tokio::test]
async fn a_temporary_thread_takes_approvals() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo", "notify"]).await;
    let owner = w.gw.client();
    let (s, v) = post(
        &w,
        &owner,
        "/chat/api/threads",
        json!({"model_alias": "chatty", "temporary": true}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let tid = v["id"].as_i64().unwrap();
    attach(&w, &owner, tid).await;
    both_calls(&w);
    let frames = send(&w, &owner, tid, "tell me").await;
    let asks = approval_frames(&frames);
    assert_eq!(asks.len(), 1, "{frames:?}");
    let id = asks[0]["approval_request_id"].clone();
    let reply = done(&frames)["message_id"].clone();
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    let last = v["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["pending_approvals"][0]["approval_request_id"], id,
        "{v}"
    );

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
    let notify = loop {
        let c = dev.next_call().await;
        if c["params"]["name"] == "notify" {
            break c;
        }
    };
    assert_eq!(
        notify["params"]["_meta"]["lmgw/approval"],
        json!({"decision": "approved", "by": {"kind": "owner", "name": "dashboard"}}),
        "{notify}"
    );
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "{v}");
    assert_eq!(msgs[1]["content"], "Done.");
    assert!(msgs[1].get("pending_approvals").is_none(), "{v}");

    // A second decision is told who made the first, as a stored thread's.
    let (s, r) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": id, "approve": false}]),
    )
    .await;
    assert_eq!(s, 409, "{r:?}");
    assert_eq!(r[0].1["code"], "approval_decided");
}
