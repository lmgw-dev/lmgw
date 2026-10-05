//! A session whose client is gone or has stopped reading (realtime design
//! §9.1, §10.4): pings and the close that names `realtime.ping_interval_s`,
//! the writer's bounded drain on the way out, a client that closed its side
//! and stopped reading, and the chat hold released when the stream ends
//! rather than when the client has read the answer.

use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use lmgw_core::config::{KeyPolicy, Settings};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;

use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_fakes::{
    chat_fake, events_until, gateway, gpu_gateway, next_event, open, send, text_session, user_text,
    ChatFake, Ws, KEY,
};

/// A user item whose `conversation.item.added` and `.done` echoes are each
/// larger than what loopback socket buffers hold: a client that does not
/// read stalls the writer on it.
fn big_item() -> serde_json::Value {
    user_text(&"x".repeat(8 << 20))
}

#[tokio::test]
async fn a_client_that_answers_no_ping_is_closed_with_the_setting_named() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |s| s.realtime.ping_interval_s = 1).await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    // Not even `session.created` is read, so nothing answers the pings.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let close = loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("the server said nothing");
        match msg {
            Some(Ok(Message::Close(c))) => break c.expect("a close frame with a reason"),
            Some(Ok(_)) => continue,
            other => panic!("expected a close frame, got {other:?}"),
        }
    };
    assert_eq!(u16::from(close.code), 1011);
    assert_eq!(
        close.reason.as_str(),
        "no pong within 1 s, setting realtime.ping_interval_s"
    );
}

#[tokio::test]
async fn a_client_that_reads_answers_the_pings_and_stays() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |s| s.realtime.ping_interval_s = 1).await;
    let mut ws = text_session(&addr).await;
    let end = Instant::now() + Duration::from_millis(3500);
    let mut pings = 0;
    while Instant::now() < end {
        // Reading is what lets the client's WebSocket answer a ping.
        match tokio::time::timeout(Duration::from_millis(100), ws.next()).await {
            Ok(Some(Ok(Message::Ping(_)))) => pings += 1,
            Ok(other) => panic!("expected only pings, got {other:?}"),
            Err(_) => {}
        }
    }
    assert!(pings >= 2, "{pings} pings");
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": "nope", "event_id": "alive"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["error"]["event_id"], "alive", "the session is still open");
}

#[tokio::test]
async fn a_client_that_stopped_reading_loses_its_socket_after_one_drain_interval() {
    let fake = chat_fake().await;
    let (_s, addr) = gateway(&fake, false, None, |s| s.realtime.ping_interval_s = 1).await;
    let mut ws = text_session(&addr).await;
    send(&mut ws, big_item()).await;
    // Two intervals for the unanswered ping, one for the writer's drain.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    // What is left of the echo arrives, and then the socket ends: the writer
    // was dropped, not left waiting for a reader.
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(_)) = ws.next().await {}
    })
    .await;
    assert!(ended.is_ok(), "the socket stayed open");
}

/// A gateway pinging every second, whose one key may hold one request at a
/// time — a session holds it for its whole life (§10.3).
async fn one_slot_gateway(tweak: impl FnOnce(&mut Settings)) -> (ChatFake, String) {
    let fake = chat_fake().await;
    let policy = KeyPolicy {
        concurrency_limit: 1,
        ..Default::default()
    };
    let (_s, addr) = gateway(&fake, true, Some(policy), |s| {
        s.realtime.ping_interval_s = 1;
        tweak(s);
    })
    .await;
    (fake, addr)
}

/// What a plain request on the key gets: 429 while a session holds the slot.
async fn key_status(addr: &str) -> u16 {
    reqwest::Client::new()
        .get(format!("http://{addr}/v1/models"))
        .header("authorization", format!("Bearer {KEY}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// A session on the key whose client read only `session.created`, then
/// filled what the session could send it: the big echo stalls the writer,
/// errors fill its window, and the session core waits for room — with the
/// rest of the client's frames still unread on the socket.
async fn stalled_session(addr: &str) -> Ws {
    let bearer = format!("Bearer {KEY}");
    let mut ws = open(
        addr,
        "/v1/realtime?model=chatty",
        &[("authorization", bearer.as_str())],
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    send(&mut ws, big_item()).await;
    for _ in 0..80 {
        ws.send(Message::text("{}")).await.unwrap();
    }
    assert_eq!(key_status(addr).await, 429, "the session holds the slot");
    ws
}

/// Wait for the key's slot to come back; fail after `within`.
async fn slot_released_within(addr: &str, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        let status = key_status(addr).await;
        if status == 200 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the session still holds the key's slot (last status {status})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Package A review #1: a close (and the FIN behind it) is the end, not an
/// answer to the ping — the session waiting for the writer used to read it
/// again on every tick and hold the slot for good.
#[tokio::test]
async fn a_client_that_closes_and_stops_reading_frees_the_slot_within_the_interval() {
    let (_fake, addr) = one_slot_gateway(|_| {}).await;
    let mut ws = stalled_session(&addr).await;
    ws.send(Message::Close(None)).await.unwrap();
    let MaybeTlsStream::Plain(tcp) = ws.get_mut() else {
        panic!("a plain test socket");
    };
    tcp.shutdown().await.unwrap();
    // The first tick pings, the second finds the close instead of a pong.
    slot_released_within(&addr, Duration::from_secs(5)).await;
    // The writer had its one interval to drain, and the socket went with it.
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(_)) = ws.next().await {}
    })
    .await;
    assert!(ended.is_ok(), "the socket stayed open");
}

/// The same with a frame over `realtime.max_message_mb`: its failed read
/// ends the read side, which then yields its end on every read.
#[tokio::test]
async fn a_client_that_sends_an_oversized_frame_and_stops_reading_frees_the_slot() {
    let (_fake, addr) = one_slot_gateway(|s| {
        s.realtime.max_message_mb = 9;
        s.realtime.max_frame_mb = 9;
    })
    .await;
    let mut ws = stalled_session(&addr).await;
    // The server refuses the frame at its header and reads no more, so the
    // send never finishes; it runs on its own and keeps the socket open.
    tokio::spawn(async move {
        let _ = ws.send(Message::text("x".repeat(10 << 20))).await;
        std::future::pending::<()>().await;
    });
    slot_released_within(&addr, Duration::from_secs(5)).await;
    // The session closes it with 1009 naming the setting, as the reader
    // loop would (A2 review 1, `session::stop`'s test) — but a client that
    // stopped reading never sees that frame, and the frame it is still
    // sending makes the socket's end a reset.
}

#[tokio::test]
async fn the_chat_hold_ends_with_the_stream_even_when_the_client_stops_reading() {
    let g = Gpu::new(10 * GIB, 2, 5).await;
    g.model("voice", 4 * GIB).await;
    // The model takes its time: the client stops reading while it answers.
    g.world().chat_delay = Duration::from_millis(1500);
    let addr = gpu_gateway(&g, |_| {}).await;
    let mut ws = open(&addr, "/v1/realtime?model=voice", &[]).await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    // From here on the client reads nothing, and fills what the session
    // could send it: an echo larger than the socket buffers stalls the
    // writer, and errors fill the writer's window behind it — the session
    // core now waits for a writer that waits for the client.
    send(&mut ws, big_item()).await;
    for _ in 0..80 {
        ws.send(Message::text("{}")).await.unwrap();
    }

    let in_flight = || {
        g.state
            .runtime()
            .list()
            .iter()
            .find(|v| v.model_id == "voice")
            .map(|v| v.in_flight)
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // The model answered, and no claim on it is left: the hold went
        // with the stream, though `response.done` has not gone out.
        if g.world().chats.len() == 1 && in_flight() == Some(0) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the hold outlived the call: in_flight {:?}, chats {}",
            in_flight(),
            g.world().chats.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The client reads again, and gets everything, the answer included.
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{:?}",
        events.last()
    );
}
