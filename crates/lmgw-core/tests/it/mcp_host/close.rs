//! How lmgw closes a link, and says why (§5.1–§5.3, L18, the "As built
//! (WP7)" note): a key deleted 4003 — never 1000, though its row goes with
//! it —, the grant cleared or the row switched off 1000, a frame over
//! `mcp.host_max_frame_mb` 1009 naming it, a missed pong 1011; and two links
//! of one device opening at once leave exactly one.

use futures::StreamExt;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

use super::{device_row, host_world, linked, mcp_tools, tool, FakeDevice};
use crate::common::patience;
use crate::device_chat::op;

/// A key deleted closes its link 4003 (L18), though the delete takes the
/// key's row with it and reloads the configuration before it revokes.
#[tokio::test]
async fn a_key_deleted_closes_its_link_4003() {
    for _ in 0..3 {
        let (w, d) = host_world().await;
        let mut dev = linked(&w, &d, &["echo"]).await;
        let (s, v) = op(&w, "key_delete", json!({"id": d.id})).await;
        assert_eq!(s, 200, "{v}");
        let (code, reason) = dev.closed().await;
        assert_eq!(code, 4003, "{reason}");
        assert!(reason.starts_with("key_unknown: "), "{reason}");
    }
}

/// The grant cleared, or the row switched off on the MCP page: 1000, saying
/// which.
#[tokio::test]
async fn a_grant_cleared_or_a_row_switched_off_closes_1000() {
    let (w, d) = host_world().await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    let id = device_row(&w, d.id).id;
    let (s, v) = op(&w, "mcp_server_set", json!({"action": "disable", "id": id})).await;
    assert_eq!(s, 200, "{v}");
    let (code, reason) = dev.closed().await;
    assert_eq!(code, 1000, "{reason}");
    assert!(reason.contains("switched off"), "{reason}");

    let (s, v) = op(&w, "mcp_server_set", json!({"action": "enable", "id": id})).await;
    assert_eq!(s, 200, "{v}");
    let mut dev = linked(&w, &d, &["echo"]).await;
    let (s, v) = op(&w, "key_set", json!({"id": d.id, "hosts_label": ""})).await;
    assert_eq!(s, 200, "{v}");
    let (code, reason) = dev.closed().await;
    assert_eq!(code, 1000, "{reason}");
    assert!(reason.contains("hosting grant was cleared"), "{reason}");
}

/// A frame over the frame limit closes 1009 naming that setting.
#[tokio::test]
async fn a_frame_overrun_names_the_frame_setting() {
    let (w, d) = host_world().await;
    crate::realtime_chat_thread::settings(&w.state, |s| {
        s.mcp.host_max_message_mb = 4;
        s.mcp.host_max_frame_mb = 1;
    })
    .await;
    let mut dev = linked(&w, &d, &["echo"]).await;
    dev.send(Message::Text("x".repeat(2 * 1024 * 1024).into()));
    let (code, reason) = dev.closed().await;
    assert_eq!(code, 1009, "{reason}");
    assert!(reason.contains("mcp.host_max_frame_mb"), "{reason}");
}

/// A device that stops reading answers no ping: the link closes 1011
/// naming the setting.
#[tokio::test]
async fn a_missed_pong_closes_1011() {
    let (w, d) = host_world().await;
    crate::realtime_chat_thread::settings(&w.state, |s| s.mcp.host_ping_interval_s = 1).await;
    let mut ws = super::device::silent(&w.addr(), &d.key).await;
    // Unread for two intervals: the first ping goes unanswered.
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
    let close = patience::within("the gateway's close", async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(frame))) => break frame,
                Some(Ok(_)) => continue,
                other => panic!("the link ended without a close: {other:?}"),
            }
        }
    })
    .await
    .expect("a close frame");
    assert_eq!(u16::from(close.code), 1011, "{close:?}");
    assert!(
        close.reason.contains("mcp.host_ping_interval_s"),
        "{close:?}"
    );
}

/// Two links of one device opening at once (review finding 5): one is
/// taken over (4000), the other serves — never both closed.
#[tokio::test]
async fn two_links_opening_at_once_leave_one() {
    let (w, d) = host_world().await;
    let id = device_row(&w, d.id).id;
    let addr = w.addr();
    for round in 0..5 {
        let (mut a, mut b) = tokio::join!(
            FakeDevice::connect(&addr, &d.key, vec![tool("a")]),
            FakeDevice::connect(&addr, &d.key, vec![tool("b")]),
        );
        let closed = patience::within("one of the two closes", async {
            tokio::select! {
                c = a.closed() => (c, "b"),
                c = b.closed() => (c, "a"),
            }
        })
        .await;
        let ((code, reason), survivor) = closed;
        assert_eq!(code, 4000, "round {round}: {reason}");
        patience::until_async("the survivor serves", || async {
            w.state.mcp.is_ready(id).await
                && mcp_tools(&w, &w.gw.client())
                    .await
                    .contains(&format!("desktop__{survivor}"))
        })
        .await;
        let (alive, gone) = if survivor == "a" { (a, b) } else { (b, a) };
        drop(gone);
        alive.vanish();
        patience::until_async("the row is offline again", || async {
            !w.state.mcp.is_ready(id).await
        })
        .await;
    }
}
