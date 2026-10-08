//! Revocation reaches a device's running work (client-apps design §1.6,
//! review W2-2): a Rotate ends a Chat stream it holds open — the turn behind
//! it stops and its upstream request is dropped — and `/api/events` says
//! when a device connects and when a key is revoked, so the Keys page needs
//! no poll.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use super::{chat_thread, device_world, frames, get, op};
use crate::realtime_chat_thread::World;
use crate::support::realtime_fakes::{Step, Turn};

/// Read `resp` until `done(buf)` holds, or fail after `secs`.
async fn read_until(
    resp: &mut reqwest::Response,
    secs: u64,
    done: impl Fn(&str) -> bool,
) -> String {
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !done(&buf) {
        let chunk = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .unwrap_or_else(|_| panic!("timed out; got so far:\n{buf}"))
            .unwrap();
        match chunk {
            Some(c) => buf.push_str(&String::from_utf8_lossy(&c)),
            None => break,
        }
    }
    buf
}

#[tokio::test]
async fn a_rotate_ends_a_device_s_running_turn() {
    let (w, d) = device_world().await;
    let hold = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Thinking"),
        Step::Wait(hold.clone()),
        Step::Text(" more."),
        Step::Finish("stop"),
        Step::Usage(12, 8),
    ]));
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let mut resp = d
        .client
        .post(format!("{}/chat/api/threads/{tid}/send", w.gw))
        .json(&json!({ "content": "a long one" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    read_until(&mut resp, 10, |b| b.contains("Thinking")).await;

    let (s, v) = op(&w, "key_rotate", json!({ "id": d.id })).await;
    assert_eq!(s, 200, "{v}");
    let rest = read_until(&mut resp, 10, |_| false).await;
    let said = frames(&rest);
    let revoked = said
        .iter()
        .find(|(e, d)| e == "error" && d["code"] == "revoked")
        .unwrap_or_else(|| panic!("no revoked frame in {rest}"));
    assert_eq!(
        revoked.1["message"],
        "device 'phone' was rotated — pair it again"
    );
    assert!(
        !rest.contains(" more."),
        "nothing after the revocation: {rest}"
    );
    // The turn behind it stopped, and dropped its upstream request.
    let stopped = tokio::time::timeout(Duration::from_secs(5), async {
        while w.chat.seen.closed_early.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(stopped.is_ok(), "the upstream stream was never dropped");
    hold.notify_one();
    // And the old key opens nothing more.
    let (s, v) = get(&w, &d.client, "/chat/api/threads").await;
    assert_eq!((s, &v["code"]), (401, &json!("device_key_unknown")), "{v}");
}

/// Disable and Delete end a device's running Chat stream as a Rotate does,
/// and so does its expiry by the clock (review W3-11) — a regenerate's
/// stream here, the same wrapper as every Chat stream.
#[tokio::test]
async fn disable_delete_and_expiry_end_a_device_s_running_turn() {
    for how in ["disable", "delete", "expire"] {
        let (w, d) = device_world().await;
        w.chat.push(Turn::text(&["First."]));
        let tid = chat_thread(&w, &d.client, "chatty").await;
        let (s, said) = super::sse(
            &w,
            &d.client,
            &format!("/chat/api/threads/{tid}/send"),
            json!({ "content": "hi" }),
        )
        .await;
        assert_eq!(s, 200);
        let reply = said
            .iter()
            .find(|(e, _)| e == "done")
            .and_then(|(_, d)| d["message_id"].as_i64())
            .unwrap();
        let hold = Arc::new(Notify::new());
        w.chat.push(Turn::Stream(vec![
            Step::Text("Thinking"),
            Step::Wait(hold.clone()),
            Step::Text(" more."),
            Step::Finish("stop"),
            Step::Usage(12, 8),
        ]));
        let mut resp = d
            .client
            .post(format!(
                "{}/chat/api/threads/{tid}/messages/{reply}/regenerate",
                w.gw
            ))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        read_until(&mut resp, 10, |b| b.contains("Thinking")).await;
        let (s, v) = match how {
            "disable" => op(&w, "key_set", json!({ "id": d.id, "enabled": false })).await,
            "delete" => op(&w, "key_delete", json!({ "id": d.id })).await,
            _ => {
                let at = (chrono::Utc::now() + chrono::Duration::seconds(1)).to_rfc3339();
                op(&w, "key_set", json!({ "id": d.id, "expires_at": at })).await
            }
        };
        assert_eq!(s, 200, "{how}: {v}");
        let rest = read_until(&mut resp, 10, |_| false).await;
        let said = frames(&rest);
        let revoked = said
            .iter()
            .find(|(e, d)| e == "error" && d["code"] == "revoked")
            .unwrap_or_else(|| panic!("{how}: no revoked frame in {rest}"));
        let want = match how {
            "disable" => "device 'phone' was disabled",
            "delete" => "device 'phone' was deleted",
            _ => "device 'phone' expired",
        };
        assert_eq!(revoked.1["message"], want, "{how}");
        // What to do, as a realtime close's token says it (review G-7).
        let kind = match how {
            "disable" => "device_disabled",
            "delete" => "key_unknown",
            _ => "key_expired",
        };
        assert_eq!(revoked.1["kind"], kind, "{how}");
        assert!(!rest.contains(" more."), "{how}: {rest}");
        hold.notify_one();
    }
}

/// Disable means disable for an owner key too (review W3-6): its running
/// Chat turn — here the dashboard's own session, rotated — ends with the
/// `revoked` frame, and the turn behind it stops.
#[tokio::test]
async fn an_owner_key_s_rotate_ends_its_running_turn() {
    let (w, _d) = device_world().await;
    let hold = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Thinking"),
        Step::Wait(hold.clone()),
        Step::Text(" more."),
        Step::Finish("stop"),
        Step::Usage(12, 8),
    ]));
    let owner = w.gw.client();
    let tid = chat_thread(&w, &owner, "chatty").await;
    let mut resp = owner
        .post(format!("{}/chat/api/threads/{tid}/send", w.gw))
        .json(&json!({ "content": "a long one" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    read_until(&mut resp, 10, |b| b.contains("Thinking")).await;

    let id = w
        .state
        .snapshot()
        .api_keys
        .iter()
        .find(|k| k.name == "owner:dashboard")
        .map(|k| k.id)
        .unwrap();
    let (s, v) = op(&w, "key_rotate", json!({ "id": id })).await;
    assert_eq!(s, 200, "{v}");
    let rest = read_until(&mut resp, 10, |_| false).await;
    let said = frames(&rest);
    let revoked = said
        .iter()
        .find(|(e, d)| e == "error" && d["code"] == "revoked")
        .unwrap_or_else(|| panic!("no revoked frame in {rest}"));
    assert_eq!(revoked.1["message"], "owner key 'dashboard' was rotated");
    // Any key but a paired device's is `revoked` (review G-7, G-9).
    assert_eq!(revoked.1["kind"], "revoked");
    assert!(!rest.contains(" more."), "{rest}");
    let stopped = tokio::time::timeout(Duration::from_secs(5), async {
        while w.chat.seen.closed_early.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(stopped.is_ok(), "the upstream stream was never dropped");
    hold.notify_one();
}

/// The owner's `/api/events`, opened.
async fn events(w: &World) -> reqwest::Response {
    let resp =
        w.gw.client()
            .get(format!("{}/api/events", w.gw))
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    resp
}

fn keys_frames(buf: &str) -> Vec<Value> {
    frames(buf)
        .into_iter()
        .filter(|(e, _)| e == "keys")
        .map(|(_, d)| d)
        .collect()
}

#[tokio::test]
async fn api_events_says_when_a_device_connects_and_when_it_is_revoked() {
    let (w, d) = device_world().await;
    let mut feed = events(&w).await;
    // The initial frames first.
    read_until(&mut feed, 10, |b| b.contains("event: updates")).await;

    // A device's realtime session is one of its links: open, then closed.
    let bearer = format!("Bearer {}", d.key);
    let tid = w.thread("chatty", json!({})).await;
    let ws = w
        .connect(
            &format!("chat_thread={tid}"),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    let link = json!({ "key_id": d.id, "what": "link" });
    let buf = read_until(&mut feed, 10, |b| keys_frames(b).contains(&link)).await;
    assert_eq!(keys_frames(&buf), vec![link.clone()]);

    // A Disable says so, and the session's close says "link" again.
    let (s, v) = op(&w, "key_set", json!({ "id": d.id, "enabled": false })).await;
    assert_eq!(s, 200, "{v}");
    let disabled = json!({ "key_id": d.id, "what": "disabled" });
    let buf = read_until(&mut feed, 10, |b| {
        let k = keys_frames(b);
        k.contains(&disabled) && k.contains(&link)
    })
    .await;
    assert!(keys_frames(&buf).contains(&disabled), "{buf}");
    drop(ws);
}
