//! Revocation is said, not implied (client-apps design §1.6, L18): a
//! device's feed ends with `revoked {reason, message, kind}` on Disable,
//! Rotate, Delete and at its key's expiry, and the stream closes after it;
//! `kind` is the token a realtime session's 4003 close starts with.

use serde_json::json;

use super::Feed;
use crate::device_chat::{op, pair};
use crate::realtime_chat_thread::world;

#[tokio::test]
async fn disable_rotate_and_delete_end_a_device_s_feed_with_revoked() {
    for (how, reason, message, kind) in [
        (
            "disable",
            "disabled",
            "device 'phone' was disabled",
            "device_disabled",
        ),
        (
            "rotate",
            "rotated",
            "device 'phone' was rotated — pair it again",
            "key_unknown",
        ),
        (
            "delete",
            "deleted",
            "device 'phone' was deleted",
            "key_unknown",
        ),
    ] {
        let w = world(|_| {}).await;
        let d = pair(&w, "phone", json!({})).await;
        let mut feed = Feed::open(&w, &d.client, "", None).await;
        let (status, v) = match how {
            "disable" => op(&w, "key_set", json!({ "id": d.id, "enabled": false })).await,
            "rotate" => op(&w, "key_rotate", json!({ "id": d.id })).await,
            _ => op(&w, "key_delete", json!({ "id": d.id })).await,
        };
        assert_eq!(status, 200, "{how}: {v}");
        feed.until(10, |_| false).await;
        assert!(feed.ended, "{how}: the stream ends");
        let last = feed.frames.last().unwrap();
        assert_eq!(last.event, "revoked", "{how}: {:#?}", feed.frames);
        assert_eq!(
            last.data,
            json!({ "reason": reason, "message": message, "kind": kind }),
            "{how}"
        );
    }
}

#[tokio::test]
async fn a_device_s_feed_ends_at_its_key_s_expiry() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let mut feed = Feed::open(&w, &d.client, "", None).await;
    let at = (chrono::Utc::now() + chrono::Duration::seconds(2)).to_rfc3339();
    let (status, v) = op(&w, "key_set", json!({ "id": d.id, "expires_at": at })).await;
    assert_eq!(status, 200, "{v}");
    feed.until(8, |_| false).await;
    let last = feed.frames.last().unwrap();
    assert_eq!(
        last.data,
        json!({ "reason": "expired", "message": "device 'phone' expired", "kind": "key_expired" }),
        "{:#?}",
        feed.frames
    );
}
