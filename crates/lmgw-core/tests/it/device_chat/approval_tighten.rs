//! A device only tightens `require_approval` (owner's decision, 2026-10-09;
//! client-apps design §6.6): a write that makes a label require approval
//! for fewer calls is `403 approval_loosen_refused`, naming the label and
//! the tool, and nothing is written; tightening and unchanged values pass;
//! the owner may do anything. Thread settings, folder defaults and
//! `apply_to_current`.

use serde_json::{json, Value};

use super::{chat_thread, device_world, get, post};
use crate::chat_ongoing::{current_ok, ongoing_folder};
use crate::realtime_chat_thread::World;

fn refused(s: u16, v: &Value, naming: &[&str]) {
    assert_eq!(
        (s, v["code"].as_str()),
        (403, Some("approval_loosen_refused")),
        "{v}"
    );
    let m = v["message"].as_str().unwrap();
    for n in naming {
        assert!(m.contains(n), "names {n}: {v}");
    }
}

fn tools(ra: Value) -> Value {
    json!({ "mcp_tools": [{ "server_label": "docs", "require_approval": ra }] })
}

fn filter(always: &[&str], never: &[&str]) -> Value {
    json!({ "always": { "tool_names": always }, "never": { "tool_names": never } })
}

async fn stored(w: &World, tid: i64) -> Value {
    let (_, t) = get(w, &w.gw.client(), &format!("/chat/api/threads/{tid}")).await;
    t["thread"]["mcp_tools"][0]["require_approval"].clone()
}

/// A thread of the device's whose `docs` label the owner set to `start`.
async fn thread_with(w: &World, d: &super::Device, start: Value) -> (i64, String) {
    let tid = chat_thread(w, &d.client, "chatty").await;
    let path = format!("/chat/api/threads/{tid}/settings");
    let (s, v) = post(w, &w.gw.client(), &path, tools(start)).await;
    assert_eq!(s, 200, "{v}");
    (tid, path)
}

#[tokio::test]
async fn a_device_never_loosens_a_thread_s_approvals() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let (tid, path) = thread_with(&w, &d, json!("always")).await;

    for loose in [json!("never"), filter(&["a"], &[]), filter(&[], &["a"])] {
        let (s, v) = post(&w, &d.client, &path, tools(loose)).await;
        refused(s, &v, &["'docs'"]);
        assert_eq!(stored(&w, tid).await, json!("always"));
    }
    // Dropping the field is "never".
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        json!({ "mcp_tools": [{ "server_label": "docs" }] }),
    )
    .await;
    refused(s, &v, &["'docs'"]);

    // Unchanged passes.
    let (s, v) = post(&w, &d.client, &path, tools(json!("always"))).await;
    assert_eq!(s, 200, "{v}");

    // A filter: dropping a tool from the always list, adding to never.
    let (s, v) = post(&w, &owner, &path, tools(filter(&["a", "b"], &[]))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, tools(filter(&["a"], &[]))).await;
    refused(s, &v, &["'docs'", "'b'"]);
    let (s, v) = post(&w, &d.client, &path, tools(filter(&["a", "b"], &["b"]))).await;
    refused(s, &v, &["'b'"]);
    // Tightening: a longer always list, then "always".
    let (s, v) = post(&w, &d.client, &path, tools(filter(&["a", "b", "c"], &[]))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, tools(json!("always"))).await;
    assert_eq!(s, 200, "{v}");

    // "never" -> anything is a tightening or unchanged.
    let (s, v) = post(&w, &owner, &path, tools(json!("never"))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, tools(json!("never"))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, tools(filter(&["a"], &[]))).await;
    assert_eq!(s, 200, "{v}");
    // The always list still gates only "a": other tools were never gated,
    // and gating one more is a tightening; widening to "never" is not.
    let (s, v) = post(&w, &d.client, &path, tools(json!("never"))).await;
    refused(s, &v, &["'a'"]);

    // The owner loosens freely.
    let (s, v) = post(&w, &owner, &path, tools(json!("never"))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(stored(&w, tid).await, json!("never"));
}

#[tokio::test]
async fn an_exception_list_may_not_grow_and_a_removed_label_comes_back_no_looser() {
    let (w, d) = device_world().await;
    // {never: [x]}: everything but x is gated.
    let (tid, path) = thread_with(&w, &d, filter(&[], &["x"])).await;
    let (s, v) = post(&w, &d.client, &path, tools(filter(&[], &["x", "y"]))).await;
    refused(s, &v, &["'docs'", "'y'"]);
    let (s, v) = post(&w, &d.client, &path, tools(filter(&["z"], &["x"]))).await;
    refused(s, &v, &["other tools"]);
    // A filter whose lists are both empty gates nothing: shrinking the
    // exceptions to none is a loosening; "always" is a tightening.
    let (s, v) = post(&w, &d.client, &path, tools(filter(&[], &[]))).await;
    refused(s, &v, &["other tools"]);
    let (s, v) = post(&w, &d.client, &path, tools(json!("always"))).await;
    assert_eq!(s, 200, "{v}");
    // Dropping the label passes, and the owner's floor stays: back at
    // "never" it is refused, as anything looser than the owner's {never:
    // [x]} is; the owner's rule itself, or stricter, passes.
    let (s, v) = post(&w, &d.client, &path, json!({ "mcp_tools": [] })).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, tools(json!("never"))).await;
    refused(s, &v, &["'docs'", "what the owner last set"]);
    let (s, v) = post(&w, &d.client, &path, tools(filter(&[], &["x", "y"]))).await;
    refused(s, &v, &["'y'", "what the owner last set"]);
    let (s, v) = post(&w, &d.client, &path, tools(filter(&[], &["x"]))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(stored(&w, tid).await, filter(&[], &["x"]));
}

#[tokio::test]
async fn a_new_label_starts_no_looser_than_its_folder_s_default() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let (s, f) = post(&w, &owner, "/chat/api/folders", json!({ "name": "F" })).await;
    assert_eq!(s, 200, "{f}");
    let fid = f["id"].as_i64().unwrap();
    let (s, t) = post(
        &w,
        &d.client,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "folder_id": fid }),
    )
    .await;
    assert_eq!(s, 200, "{t}");
    let tid = t["id"].as_i64().or(t["thread"]["id"].as_i64()).unwrap();
    // The folder's default then asks for approvals on docs.
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/folders/{fid}"),
        json!({ "defaults": { "model_alias": "chatty", "mcp_tools":
            [{ "server_label": "docs", "require_approval": "always" }] } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let path = format!("/chat/api/threads/{tid}/settings");
    let (s, v) = post(&w, &d.client, &path, tools(json!("never"))).await;
    refused(s, &v, &["'docs'", "folder's default"]);
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        json!({ "mcp_tools": [{ "server_label": "docs" }] }),
    )
    .await;
    refused(s, &v, &["'docs'"]);
    let (s, v) = post(&w, &d.client, &path, tools(json!("always"))).await;
    assert_eq!(s, 200, "{v}");
}

#[tokio::test]
async fn folder_defaults_follow_the_same_rule() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let body = |ra: Value| json!({ "name": "F", "defaults": tools(ra) });
    // A new folder's defaults have no baseline.
    let (s, f) = post(&w, &d.client, "/chat/api/folders", body(json!("never"))).await;
    assert_eq!(s, 200, "{f}");
    let fid = f["id"].as_i64().unwrap();
    let path = format!("/chat/api/folders/{fid}");
    let set = |ra: Value| {
        json!({ "defaults": { "model_alias": "chatty", "mcp_tools":
        [{ "server_label": "docs", "require_approval": ra }] } })
    };
    let (s, v) = post(&w, &d.client, &path, set(json!("always"))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, set(json!("never"))).await;
    refused(s, &v, &["'docs'"]);
    let (s, v) = post(&w, &d.client, &path, set(filter(&["a"], &[]))).await;
    refused(s, &v, &["other tools"]);
    let (s, v) = post(&w, &d.client, &path, set(json!("always"))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &owner, &path, set(json!("never"))).await;
    assert_eq!(s, 200, "{v}");
}

#[tokio::test]
async fn apply_to_current_cannot_loosen_the_current_thread_either() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let fid = ongoing_folder(&w, &owner, "Assistant", 0, tools(json!("always"))).await;
    let (current, _, _) = current_ok(&w, &owner, fid, false).await;
    let path = format!("/chat/api/folders/{fid}");
    let set = |ra: Value, apply: bool| {
        json!({ "apply_to_current": apply, "defaults": { "model_alias":
        "chatty", "mcp_tools": [{ "server_label": "docs", "require_approval": ra }] } })
    };

    // The folder's own defaults are refused first, applied or not.
    let (s, v) = post(&w, &d.client, &path, set(json!("never"), true)).await;
    refused(s, &v, &["'docs'"]);
    let (s, v) = post(&w, &d.client, &path, set(json!("never"), false)).await;
    refused(s, &v, &["'docs'"]);
    assert_eq!(stored(&w, current).await, json!("always"));

    // The owner loosens the folder alone; the thread still gates. A device's
    // tightening of the folder then reaches the thread, and the thread's own
    // looser value (set by the owner) is not the device's to keep loosening.
    let (s, v) = post(&w, &owner, &path, set(json!("never"), false)).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, set(json!("always"), true)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(stored(&w, current).await, json!("always"));
}

/// `apply_to_current` reaches the current thread's own check: a folder
/// default the folder never had passes the folder's, and the thread's
/// stricter rule refuses it — the whole patch, folder included.
#[tokio::test]
async fn apply_to_current_meets_the_thread_s_own_rule() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let fid = ongoing_folder(&w, &owner, "Assistant", 0, json!({})).await;
    let (current, _, _) = current_ok(&w, &owner, fid, false).await;
    let (s, v) = post(
        &w,
        &owner,
        &format!("/chat/api/threads/{current}/settings"),
        tools(json!("always")),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let path = format!("/chat/api/folders/{fid}");
    let set = |apply: bool| {
        json!({ "apply_to_current": apply, "defaults": { "model_alias": "chatty",
        "mcp_tools": [{ "server_label": "docs", "require_approval": "never" }] } })
    };
    let (s, v) = post(&w, &d.client, &path, set(true)).await;
    refused(s, &v, &["'docs'", "what the owner last set"]);
    assert_eq!(stored(&w, current).await, json!("always"));
    let (_, list) = get(&w, &owner, "/chat/api/folders").await;
    let f = list["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == fid)
        .unwrap();
    assert!(
        f["defaults"]["mcp_tools"].is_null(),
        "the folder was not changed either: {f}"
    );
    // Without applying, the folder's own default is the device's to set.
    let (s, v) = post(&w, &d.client, &path, set(false)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(stored(&w, current).await, json!("always"));
}
