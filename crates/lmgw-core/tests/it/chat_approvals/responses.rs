//! `/v1/responses`' continuation decides too (client-apps design §6.6, the
//! stamping rule): `by` is the request's own principal, and a sibling held
//! beside the gated call carries nobody.

use serde_json::{json, Value};

use super::{approved_by, both_calls};
use crate::mcp_host::{host_world, linked};
use crate::support::realtime_fakes::Turn;

/// A streamed `/v1/responses` call as `client` (the fake upstream
/// streams): the response its `response.completed` carries.
async fn respond(
    w: &crate::realtime_chat_thread::World,
    client: &reqwest::Client,
    mut body: Value,
) -> Value {
    body["stream"] = json!(true);
    let resp = client
        .post(format!("{}/v1/responses", w.gw))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let text = resp.text().await.unwrap();
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .find(|e| e["type"] == "response.completed")
        .map(|e| e["response"].clone())
        .unwrap_or_else(|| panic!("no response.completed in {text}"))
}

/// A device's response stops on its gated `notify`; its continuation
/// approving it runs the call with the device as approver — on the row and
/// in `_meta` — while the sibling `echo` carries nobody.
#[tokio::test]
async fn a_responses_continuation_names_its_own_principal() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo", "notify"]).await;
    let tools = json!([{"type": "mcp", "server_label": "desktop",
                        "require_approval": {"always": {"tool_names": ["notify"]}}}]);
    both_calls(&w);
    let asked = respond(
        &w,
        &d.client,
        json!({"model": "chatty", "input": "tell me", "tools": tools}),
    )
    .await;
    let request = asked["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "mcp_approval_request")
        .unwrap_or_else(|| panic!("no approval request in {asked:#}"))
        .clone();

    w.chat.push(Turn::text(&["Done."]));
    respond(
        &w,
        &d.client,
        json!({"model": "chatty", "tools": tools, "previous_response_id": asked["id"],
               "input": [{"type": "mcp_approval_response",
                          "approval_request_id": request["id"], "approve": true}]}),
    )
    .await;
    let mut seen = [dev.next_call().await, dev.next_call().await];
    seen.sort_by_key(|c| c["params"]["name"].as_str().unwrap().to_string());
    let (echo, notify) = (&seen[0], &seen[1]);
    let device = json!({"kind": "device", "name": "desktop"});
    assert_eq!(notify["params"]["_meta"]["lmgw/caller"], device, "{notify}");
    assert_eq!(
        notify["params"]["_meta"]["lmgw/approval"],
        json!({"decision": "approved", "by": device}),
        "{notify}"
    );
    assert_eq!(
        echo["params"]["_meta"]["lmgw/approval"],
        Value::Null,
        "a sibling nobody approved: {echo}"
    );
    assert_eq!(
        approved_by(&w, "desktop__notify").await,
        vec![Some("device:desktop".to_string())]
    );
    assert_eq!(approved_by(&w, "desktop__echo").await, vec![None]);
}
