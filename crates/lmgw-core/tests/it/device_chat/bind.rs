//! Binding a voice session from a device (client-apps design §1.3, §1.7):
//! `Chat` binds; an Admin Chat thread does not exist for it; the thread's
//! aliases pass its key's policy before the 101; its refusals write rows;
//! the session it takes over is told who took it.

use futures::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::{admin_thread, pair, rows_of};
use crate::realtime_chat_thread::{next, world};

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn a_device_binds_a_chat_thread_and_not_an_admin_one() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let bearer = format!("Bearer {}", d.key);
    let auth = [("authorization", bearer.as_str())];
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w
        .connect(&format!("chat_thread={tid}"), &auth)
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    let created = next(&mut ws).await;
    assert_eq!(created["type"], "session.created", "{created}");
    assert_eq!(
        created["session"]["lmgw"]["resolved"]["chat_thread"]["id"],
        json!(tid)
    );

    // An Admin Chat thread is no thread at all for a device (L3) — and the
    // refusal is its request row, as realtime's own handshake refusals are.
    let admin = admin_thread(&w).await;
    let (status, body) = w
        .connect(&format!("chat_thread={}", admin.id), &auth)
        .await
        .unwrap_err();
    assert_eq!(
        (status, code(&body)),
        (404, "chat_thread_not_found"),
        "{body}"
    );
    let rows = rows_of(&w, d.id).await;
    assert_eq!(
        rows.last().map(|r| (r.0.as_str(), r.2)),
        Some(("realtime", 404))
    );

    // The owner's own refusal stays the dashboard's: no row (chat-voice
    // §8.8, NIT 12), and an admin thread is still the 409 it was.
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_logs")
        .fetch_one(&w.state.db)
        .await
        .unwrap();
    let cookie = w.cookie();
    let (status, body) = w
        .connect(
            &format!("chat_thread={}", admin.id),
            &[("cookie", cookie.as_str())],
        )
        .await
        .unwrap_err();
    assert_eq!((status, code(&body)), (409, "chat_thread_admin"), "{body}");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_logs")
        .fetch_one(&w.state.db)
        .await
        .unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn the_thread_s_aliases_pass_the_device_s_policy_before_the_101() {
    let w = world(|_| {}).await;
    // Chat and ASR in scope, the thread's TTS (`speak`) not.
    let d = pair(
        &w,
        "phone",
        json!({ "scope_mode": "allow", "scope_patterns": "chatty\nhear" }),
    )
    .await;
    let bearer = format!("Bearer {}", d.key);
    let tid = w.thread("chatty", json!({})).await;
    let (status, body) = w
        .connect(
            &format!("chat_thread={tid}"),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_err();
    assert_eq!((status, code(&body)), (403, "key_scope"), "{body}");
    assert!(body.to_string().contains("speak"), "{body}");
    let rows = rows_of(&w, d.id).await;
    assert_eq!(
        rows.iter()
            .map(|r| (r.0.as_str(), r.1.as_str(), r.2))
            .collect::<Vec<_>>(),
        vec![("realtime", "speak", 403)]
    );
}

#[tokio::test]
async fn a_takeover_names_the_device_that_took_the_thread() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let bearer = format!("Bearer {}", d.key);
    let tid = w.thread("chatty", json!({})).await;
    let (mut dashboard, _) = w.bind(tid).await;
    let _phone = w
        .connect(
            &format!("chat_thread={tid}"),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    let e = next(&mut dashboard).await;
    assert_eq!(code(&e), "chat_thread_taken_over", "{e}");
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("voice mode moved to device 'phone'"),
        "{e}"
    );
    let close = loop {
        match dashboard.next().await {
            Some(Ok(Message::Close(c))) => break c,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Text(_))) => continue,
            other => panic!("expected the close, got {other:?}"),
        }
    };
    let close = close.expect("a close frame with a reason");
    assert_eq!(u16::from(close.code), 4000);
    assert_eq!(close.reason.as_str(), "voice mode moved to device 'phone'");
}

/// The other direction (review W3-11): the dashboard takes a device's
/// session over, and the device is told it moved to the dashboard. The
/// feed's `voice.ended` names both binders (review W5-6, the W4-13 residue).
#[tokio::test]
async fn a_device_taken_over_is_told_it_moved_to_the_dashboard() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    let bearer = format!("Bearer {}", d.key);
    let tid = w.thread("chatty", json!({})).await;
    let mut feed = crate::chat_feed::Feed::open(&w, &w.gw.client(), "", None).await;
    let mut phone = w
        .connect(
            &format!("chat_thread={tid}"),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    let created = next(&mut phone).await;
    assert_eq!(created["type"], "session.created", "{created}");
    let (_dashboard, _) = w.bind(tid).await;
    let close = loop {
        match phone.next().await {
            Some(Ok(Message::Close(c))) => break c,
            Some(Ok(_)) => continue,
            other => panic!("expected the close, got {other:?}"),
        }
    };
    let close = close.expect("a close frame with a reason");
    assert_eq!(u16::from(close.code), 4000);
    assert_eq!(close.reason.as_str(), "voice mode moved to the dashboard");
    feed.until(10, |f| f.iter().any(|f| f.event == "voice.ended"))
        .await;
    let ended = feed.named("voice.ended")[0].data.clone();
    assert_eq!(
        ended,
        json!({ "thread_id": tid, "by": "device 'phone'", "reason": "taken_over",
                "taken_over_by": "the dashboard" })
    );
}

/// `takeover=never` (2026-10-07): a bind that must not take the voice from
/// another device. With two keys: the phone holds the thread, the desktop's
/// automatic rebind is refused before the 101 — 409 chat_thread_bound,
/// naming the phone as a takeover names a binder — and the phone's session
/// goes on. Without the parameter, the desktop's bind takes over, as
/// always. A bind with it on a free thread binds.
#[tokio::test]
async fn a_bind_that_never_takes_over_is_refused_while_another_holds_the_thread() {
    let w = world(|_| {}).await;
    let phone = pair(&w, "phone", json!({})).await;
    let desktop = pair(&w, "desktop", json!({})).await;
    let as_phone = format!("Bearer {}", phone.key);
    let as_desktop = format!("Bearer {}", desktop.key);
    let tid = w.thread("chatty", json!({})).await;
    let other = w.thread("chatty", json!({})).await;
    let mut held = w
        .connect(
            &format!("chat_thread={tid}"),
            &[("authorization", as_phone.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut held).await["type"], "session.created");

    let (status, body) = w
        .connect(
            &format!("chat_thread={tid}&takeover=never"),
            &[("authorization", as_desktop.as_str())],
        )
        .await
        .unwrap_err();
    assert_eq!((status, code(&body)), (409, "chat_thread_bound"), "{body}");
    let e = lmgw_client::requests::read_refusal(status, &body.to_string());
    assert_eq!(e.code, "chat_thread_bound");
    assert!(
        e.message.starts_with("voice is in use on device 'phone'"),
        "{}",
        e.message
    );
    // The phone's session was not touched: it still answers.
    crate::support::realtime_fakes::send(&mut held, json!({"type": "input_audio_buffer.clear"}))
        .await;
    let ev = next(&mut held).await;
    assert_eq!(ev["type"], "input_audio_buffer.cleared", "{ev}");

    // A free thread binds with it.
    let mut free = w
        .connect(
            &format!("chat_thread={other}&takeover=never"),
            &[("authorization", as_desktop.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut free).await["type"], "session.created");

    // Without it, the desktop's bind takes over, as always.
    let _taken = w
        .connect(
            &format!("chat_thread={tid}"),
            &[("authorization", as_desktop.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    let close = loop {
        match held.next().await {
            Some(Ok(Message::Close(c))) => break c,
            Some(Ok(_)) => continue,
            other => panic!("expected the close, got {other:?}"),
        }
    };
    let close = close.expect("a close frame");
    assert_eq!(u16::from(close.code), 4000);
    assert_eq!(
        close.reason.as_str(),
        "voice mode moved to device 'desktop'"
    );

    // The parameter applies to a bound session only.
    let (status, body) = w
        .connect("takeover=never", &[("authorization", as_desktop.as_str())])
        .await
        .unwrap_err();
    assert_eq!((status, code(&body)), (400, "invalid_value"), "{body}");
}

/// Review F-5: `takeover=never` is not refused by the device's own session
/// — its link dropped and the gateway has not seen it yet — and takes it
/// over as any takeover does (4000, naming the device).
#[tokio::test]
async fn a_bind_that_never_takes_over_takes_its_own_key_s_session() {
    let w = world(|_| {}).await;
    let phone = pair(&w, "phone", json!({})).await;
    let as_phone = format!("Bearer {}", phone.key);
    let tid = w.thread("chatty", json!({})).await;
    let mut first = w
        .connect(
            &format!("chat_thread={tid}"),
            &[("authorization", as_phone.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut first).await["type"], "session.created");
    let mut again = w
        .connect(
            &format!("chat_thread={tid}&takeover=never"),
            &[("authorization", as_phone.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("not refused in its own name: {s}: {b}"));
    assert_eq!(next(&mut again).await["type"], "session.created");
    let close = loop {
        match first.next().await {
            Some(Ok(Message::Close(c))) => break c.expect("a close frame"),
            Some(Ok(_)) => continue,
            other => panic!("expected the close, got {other:?}"),
        }
    };
    assert_eq!(u16::from(close.code), 4000);
    assert_eq!(close.reason.as_str(), "voice mode moved to device 'phone'");
}

/// Review F-9: two binds that never take over, at once, on a free thread:
/// exactly one binds; the other is told who holds it.
#[tokio::test]
async fn two_binds_that_never_take_over_at_once_bind_exactly_one() {
    let w = world(|_| {}).await;
    let phone = pair(&w, "phone", json!({})).await;
    let desktop = pair(&w, "desktop", json!({})).await;
    let as_phone = format!("Bearer {}", phone.key);
    let as_desktop = format!("Bearer {}", desktop.key);
    for _ in 0..5 {
        let tid = w.thread("chatty", json!({})).await;
        let query = format!("chat_thread={tid}&takeover=never");
        let ha = [("authorization", as_phone.as_str())];
        let hb = [("authorization", as_desktop.as_str())];
        let (a, b) = tokio::join!(w.connect(&query, &ha), w.connect(&query, &hb));
        let refused: Vec<(u16, Value)> = [&a, &b]
            .into_iter()
            .filter_map(|r| r.as_ref().err().cloned())
            .collect();
        assert_eq!(refused.len(), 1, "exactly one bound");
        let (status, body) = &refused[0];
        assert_eq!((*status, code(body)), (409, "chat_thread_bound"), "{body}");
        let holder = if a.is_ok() { "phone" } else { "desktop" };
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.starts_with(&format!("voice is in use on device '{holder}'")),
            "{message}"
        );
    }
}
