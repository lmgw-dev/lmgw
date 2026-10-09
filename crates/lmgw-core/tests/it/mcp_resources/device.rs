//! A device's resources over its host link: its own `resources/list_changed`
//! reaches `/mcp`, and a read goes as a call does — cancelled on the device
//! at the row's timeout, when lmgw stops waiting, and before lmgw closes the
//! link, the read then saying why.

use std::sync::atomic::Ordering;

use futures::StreamExt;
use serde_json::{json, Value};

use lmgw_core::mcp::host::CallFrom;
use lmgw_core::mcp::scope::ToolScope;

use super::rpc;
use crate::common::patience;
use crate::device_chat::op;
use crate::mcp_host::{host_world, linked, mcp_session, set_timeout, FakeDevice};
use crate::realtime_chat_thread::World;

/// The device's resource, as `/mcp` names it.
const URI: &str = "ui://desktop__panel/main";

/// `desktop` linked, listing its panel, its reads left unanswered.
async fn holding(w: &World, d: &crate::device_chat::Device) -> FakeDevice {
    let dev = linked(w, d, &["see"]).await;
    dev.set_resources(vec![json!({"uri": "ui://panel/main", "name": "panel"})]);
    dev.hold_reads.store(true, Ordering::SeqCst);
    dev
}

/// The next `notifications/cancelled` the device got, whole.
async fn cancelled(dev: &mut FakeDevice) -> Value {
    dev.next_note("notifications/cancelled").await
}

/// A device saying its resources changed: `/mcp`'s subscribers hear it,
/// on the `GET /mcp` stream too.
#[tokio::test]
async fn a_device_s_own_resources_list_changed_reaches_mcp() {
    let (w, d) = host_world().await;
    let dev = linked(&w, &d, &["see"]).await;
    let mut changed = w.state.mcp.subscribe_resources_changed();
    let owner = w.gw.client();
    let sid = mcp_session(&w, &owner).await;
    let resp = owner
        .get(format!("{}/mcp", w.gw))
        .header("mcp-session-id", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    dev.send_json(json!({"jsonrpc": "2.0", "method": "notifications/resources/list_changed"}));
    patience::within("the upstream's resources/list_changed", changed.recv())
        .await
        .expect("the signal");
    let mut body = resp.bytes_stream();
    let saw = patience::within("the stream's resources/list_changed", async {
        let mut buf = String::new();
        while let Some(chunk) = body.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if buf.contains("notifications/resources/list_changed") {
                return true;
            }
        }
        false
    })
    .await;
    assert!(saw, "the stream never said the resources changed");
}

/// A read the device does not answer within the row's `timeout_ms` fails
/// with the timeout, and the device is told to cancel it.
#[tokio::test]
async fn a_read_past_the_timeout_is_cancelled_on_the_device() {
    let (w, d) = host_world().await;
    set_timeout(&w, d.id, 300).await;
    let mut dev = holding(&w, &d).await;
    let v = rpc(&w, &w.gw.client(), "resources/read", json!({"uri": URI})).await;
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("timed out after 300ms"),
        "{v}"
    );
    let read = patience::within("the device's read", dev.reads.recv())
        .await
        .unwrap();
    assert_eq!(read["params"]["_meta"]["lmgw/timeout_ms"], 300, "{read}");
    let note = cancelled(&mut dev).await;
    assert_eq!(note["params"]["requestId"], read["id"], "{note}");
    assert!(
        note["params"]["reason"]
            .as_str()
            .unwrap()
            .contains("timeout_ms"),
        "{note}"
    );
}

/// A read lmgw stops waiting on — its caller gone, the read's future
/// dropped — is cancelled on the device.
#[tokio::test]
async fn a_read_lmgw_stops_waiting_on_is_cancelled_on_the_device() {
    let (w, d) = host_world().await;
    let mut dev = holding(&w, &d).await;
    let state = w.state.clone();
    let pending = tokio::spawn(async move {
        state
            .mcp
            .read_resource(
                &state.snapshot(),
                URI,
                &ToolScope::gateway(),
                &CallFrom::gateway(),
            )
            .await
            .map_err(|e| e.to_string())
    });
    let read = patience::within("the device's read", dev.reads.recv())
        .await
        .unwrap();
    pending.abort();
    let note = cancelled(&mut dev).await;
    assert_eq!(note["params"]["requestId"], read["id"], "{note}");
}

/// A link lmgw closes under a read — the key switched off — cancels the
/// read on the device first, and the read says why lmgw closed it.
#[tokio::test]
async fn a_link_lmgw_closes_under_a_read_cancels_it_first() {
    let (w, d) = host_world().await;
    let mut dev = holding(&w, &d).await;
    let owner = w.gw.client();
    let sid = mcp_session(&w, &owner).await;
    let pending = {
        let request = owner
            .post(format!("{}/mcp", w.gw))
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", sid)
            .json(
                &json!({"jsonrpc": "2.0", "id": 2, "method": "resources/read",
                          "params": {"uri": URI}}),
            );
        tokio::spawn(async move {
            let text = request.send().await.unwrap().text().await.unwrap();
            serde_json::from_str::<Value>(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
        })
    };
    let read = patience::within("the device's read", dev.reads.recv())
        .await
        .unwrap();
    let (s, v) = op(&w, "key_set", json!({"id": d.id, "enabled": false})).await;
    assert_eq!(s, 200, "{v}");
    let (code, _) = dev.closed().await;
    assert_eq!(code, 4003);
    // Sent before the close frame, so already read.
    let note: Value = loop {
        match dev.notes.try_recv() {
            Ok(n) if n["method"] == "notifications/cancelled" => break n,
            Ok(_) => continue,
            Err(e) => panic!("no cancel before the close: {e:?}"),
        }
    };
    assert_eq!(note["params"]["requestId"], read["id"], "{note}");
    let answer = patience::within("the read's answer", pending)
        .await
        .unwrap();
    let message = answer["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("lmgw closed device 'desktop''s host link")
            && message.contains("device_disabled: "),
        "{answer}"
    );
}
