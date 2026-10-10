//! The Chat round trip (client-apps design §6.2, §6.3).

use serde_json::{json, Value};

use super::{
    approval_frames, approved_by, attach, both_calls, decide, done, no_call, send, tool_results,
};
use crate::device_chat::{chat_thread, post};
use crate::mcp_host::{host_world, linked};
use crate::support::realtime_fakes::Turn;

/// A gated turn stops on its calls — the gated one announced, the sibling
/// held with it — and saves its reply with them; a decision resumes it as
/// its continuation: both calls run, the approved one carrying its
/// approver in `_meta` and on its row, the sibling neither, and the
/// model's answer is appended to the same reply.
#[tokio::test]
async fn a_gated_turn_stops_and_a_decision_resumes_it_with_its_sibling() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo", "notify"]).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    attach(&w, &owner, tid).await;
    both_calls(&w);
    let frames = send(&w, &owner, tid, "tell me").await;
    // The gated turn's frames — the `approval` tool frame and the `done`
    // that lists what waits — are the documented types.
    crate::chat_turn_wire::typed_frames(&frames);

    let asks = approval_frames(&frames);
    assert_eq!(asks.len(), 1, "{frames:?}");
    let ask = &asks[0];
    let id = ask["approval_request_id"].as_str().unwrap().to_string();
    assert!(id.starts_with("mcpr_"), "{ask}");
    assert_eq!(ask["server_label"], "desktop");
    assert_eq!(ask["name"], "notify", "the tool's own name: {ask}");
    assert_eq!(ask["arguments"], r#"{"text":"hi"}"#, "a JSON string: {ask}");
    // The call's id, as its `ready` frame carries it: a client matches the
    // approval to the call by it.
    let ready = frames
        .iter()
        .find(|(e, d)| e == "tool" && d["event"] == "ready" && d["name"] == "desktop__notify")
        .map(|(_, d)| d.clone())
        .unwrap_or_else(|| panic!("no ready frame: {frames:?}"));
    assert_eq!(ready["call_id"], "call_2", "{ready}");
    assert_eq!(ask["call_id"], ready["call_id"], "{ask}");
    let end = done(&frames);
    assert_eq!(end["aborted"], false, "{end}");
    assert_eq!(end["pending_approvals"][0]["approval_request_id"], id);
    assert_eq!(end["pending_approvals"][0]["call_id"], "call_2");
    let reply = end["message_id"].as_i64().unwrap();
    no_call(&mut dev).await;

    // The thread's read lists what waits.
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    let last = v["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["id"], reply);
    assert_eq!(last["pending_approvals"][0]["approval_request_id"], id);
    assert_eq!(last["pending_approvals"][0]["call_id"], "call_2");

    w.chat.push(Turn::text(&["Done."]));
    let (s, frames) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": id, "approve": true}]),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    // The resumed turn streams the same frame types.
    crate::chat_turn_wire::typed_frames(&frames);
    let end = done(&frames);
    assert_eq!(end["message_id"], reply, "appended to the gated reply");
    assert_eq!(end["aborted"], false, "{end}");
    assert!(end.get("pending_approvals").is_none(), "{end}");

    let mut seen = [dev.next_call().await, dev.next_call().await];
    seen.sort_by_key(|c| c["params"]["name"].as_str().unwrap().to_string());
    let (echo, notify) = (&seen[0], &seen[1]);
    assert_eq!(echo["params"]["name"], "echo");
    assert_eq!(
        echo["params"]["_meta"]["lmgw/approval"],
        Value::Null,
        "a sibling nobody approved: {echo}"
    );
    assert_eq!(
        notify["params"]["_meta"]["lmgw/approval"],
        json!({"decision": "approved", "by": {"kind": "owner", "name": "dashboard"}}),
        "{notify}"
    );
    assert_eq!(
        approved_by(&w, "desktop__notify").await,
        vec![Some("owner:dashboard".to_string())]
    );
    assert_eq!(approved_by(&w, "desktop__echo").await, vec![None]);

    // The model got both results, in its order, then answered.
    let results = tool_results(&w, 1);
    assert_eq!(
        results
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["call_1", "call_2"]
    );
    assert!(results[1].1.contains("ran notify"), "{results:?}");

    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "one user message, one reply: {v}");
    assert_eq!(msgs[1]["content"], "Done.");
    assert!(msgs[1].get("pending_approvals").is_none(), "{v}");

    // A second decision on the call is told who made the first.
    let (s, refusal) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": id, "approve": false}]),
    )
    .await;
    assert_eq!(s, 409, "{refusal:?}");
    assert_eq!(refusal[0].1["code"], "approval_decided");
    assert!(
        refusal[0].1["message"]
            .as_str()
            .unwrap()
            .contains("the dashboard"),
        "{refusal:?}"
    );
}

/// A declined call never reaches the device, and the model reads why, as
/// `/v1/responses` words it; the sibling still runs.
#[tokio::test]
async fn a_declined_call_is_answered_with_its_reason() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo", "notify"]).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    attach(&w, &owner, tid).await;
    both_calls(&w);
    let frames = send(&w, &owner, tid, "tell me").await;
    let id = approval_frames(&frames)[0]["approval_request_id"].clone();
    w.chat.push(Turn::text(&["Fine."]));
    let (s, frames) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": id, "approve": false, "reason": "not now"}]),
    )
    .await;
    assert_eq!(s, 200, "{frames:?}");
    let only = dev.next_call().await;
    assert_eq!(only["params"]["name"], "echo", "{only}");
    no_call(&mut dev).await;
    let results = tool_results(&w, 1);
    assert!(
        results[1]
            .1
            .contains("The user declined this tool call: not now"),
        "{results:?}"
    );
}

/// The route's refusals: every waiting call needs a verdict, an id nothing
/// waits under is not found, and nothing is decided by a refused request.
#[tokio::test]
async fn a_missing_verdict_and_an_unknown_id_are_refused() {
    let (w, d) = host_world().await;
    let _dev = linked(&w, &d, &["echo", "notify"]).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "desktop", "require_approval": "always"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    both_calls(&w);
    let frames = send(&w, &owner, tid, "tell me").await;
    let asks = approval_frames(&frames);
    assert_eq!(asks.len(), 2, "both gated: {frames:?}");
    let first = asks[0]["approval_request_id"].clone();

    let (s, r) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": first, "approve": true}]),
    )
    .await;
    assert_eq!(s, 400, "{r:?}");
    assert_eq!(r[0].1["code"], "approval_missing");
    let other = asks[1]["approval_request_id"].as_str().unwrap();
    assert!(r[0].1["message"].as_str().unwrap().contains(other), "{r:?}");

    let (s, r) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": "mcpr_nothing", "approve": true}]),
    )
    .await;
    assert_eq!(s, 404, "{r:?}");
    assert_eq!(r[0].1["code"], "approval_not_found");

    // Still waiting: nothing was decided.
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    let last = v["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["pending_approvals"].as_array().unwrap().len(), 2);
}

/// A new message declines what still waits, so the transcript stays valid:
/// the model's next request carries a result for every call, and a late
/// decision is told it was decided.
#[tokio::test]
async fn a_new_message_declines_what_waits() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo", "notify"]).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    attach(&w, &owner, tid).await;
    both_calls(&w);
    let frames = send(&w, &owner, tid, "tell me").await;
    let id = approval_frames(&frames)[0]["approval_request_id"].clone();

    w.chat.push(Turn::text(&["Sure."]));
    let frames = send(&w, &owner, tid, "never mind").await;
    assert_eq!(done(&frames)["aborted"], false, "{frames:?}");
    no_call(&mut dev).await;
    let results = tool_results(&w, 1);
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        results[0].1.contains("the user moved on without deciding"),
        "the sibling: {results:?}"
    );
    assert!(
        results[1]
            .1
            .contains("The user declined this tool call: the user moved on without deciding"),
        "{results:?}"
    );
    let (s, r) = decide(
        &w,
        &owner,
        tid,
        json!([{"approval_request_id": id, "approve": true}]),
    )
    .await;
    assert_eq!(s, 409, "{r:?}");
    assert_eq!(r[0].1["code"], "approval_decided");
}

/// `require_approval` reads as `/v1/responses` reads it: `read_only` and
/// an unknown value are refused on write, for the owner too.
#[tokio::test]
async fn require_approval_is_checked_when_it_is_written() {
    let (w, _d) = host_world().await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    for bad in [
        json!("sometimes"),
        json!({"always": {"read_only": true}}),
        json!(3),
    ] {
        let (s, v) = post(
            &w,
            &owner,
            &format!("/chat/api/threads/{tid}/settings"),
            json!({"mcp_tools": [{"server_label": "desktop", "require_approval": bad}]}),
        )
        .await;
        assert_eq!(s, 400, "{bad}: {v}");
    }
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({"mcp_tools": [{"server_label": "desktop", "require_approval": "never"}]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let v = w.get(&format!("/chat/api/threads/{tid}")).await;
    assert_eq!(
        v["thread"]["mcp_tools"][0]["require_approval"], "never",
        "{v}"
    );
}
