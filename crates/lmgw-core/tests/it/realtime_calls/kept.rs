//! The audio kept before `oai-events` opens (§4.6): counted against the
//! call's budget of what waits for its owner (`realtime.max_message_mb`), so
//! a client that streams and never opens the channel is ended past it; and
//! let go when the call goes pending, so it no longer counts while the user
//! decides.
//!
//! Both worlds set `realtime.ping_interval_s` to 0: no never-opened bound,
//! so the budget is the only end a call meets here.

use std::time::Duration;

use serde_json::json;
use str0m::{Event, Rtc};

use super::ask::holder;
use super::{calls_world, post_sdp, Phone};
use crate::common::patience::within;
use crate::realtime_calls_net::client::Client;
use crate::realtime_chat_thread::World;

/// A frame's payload: not Opus (nothing decodes it before WP3), 1000 bytes.
const PAYLOAD: [u8; 1000] = [7; 1000];

/// A phone whose offer carries a channel that is not `oai-events`, answered
/// for `query` by the owner and connected: ICE and DTLS up, nothing open for
/// the call. Its send buffer takes a message as large as lmgw receives.
async fn unopened(w: &World, query: &str) -> Phone {
    let builder = Rtc::builder().set_sctp_max_buffered_amount(512 << 10);
    let ip = "127.0.0.1".parse().unwrap();
    let (c, sdp) = Client::labelled_with(ip, builder, "not-yet").await;
    let a = post_sdp(w, &w.gw.client(), query, &sdp).await;
    let mut p = Phone::answered(c, a);
    p.c.connect().await;
    p
}

/// Send `frames` audio frames of [`PAYLOAD`], a few at a time with the
/// client driven between, so loopback's buffers keep up.
async fn stream(c: &mut Client, frames: usize) {
    for k in 0..frames {
        c.send_frame_now(&PAYLOAD);
        if k % 16 == 15 {
            c.run_for(Duration::from_millis(2)).await;
        }
    }
}

#[tokio::test]
async fn audio_kept_before_the_open_past_the_budget_ends_the_call() {
    let w = calls_world(|s| {
        s.realtime.ping_interval_s = 0;
        s.realtime.max_message_mb = 1;
    })
    .await;
    let mut p = unopened(&w, "model=chatty").await;
    // Faster than real time, and never an open: only the budget ends it.
    within("lmgw ended the call past the budget", async {
        loop {
            stream(&mut p.c, 16).await;
            if p.c.met(|e| matches!(e, Event::Closed)) {
                return;
            }
        }
    })
    .await;
    assert!(w.state.calls.get(&p.id).is_none());
}

#[tokio::test]
async fn audio_kept_before_the_open_goes_when_the_call_is_pending() {
    let w = calls_world(|s| {
        s.realtime.ping_interval_s = 0;
        s.realtime.max_message_mb = 1;
    })
    .await;
    let tid = w.thread("chatty", json!({})).await;
    let _desktop = holder(&w, tid, "desktop").await;
    let mut p = unopened(&w, &format!("chat_thread={tid}&takeover=ask")).await;
    // 900 000 bytes kept, under the budget's 1 048 576.
    stream(&mut p.c, 900).await;
    p.c.open_events().await;
    let first = p.next_but_states().await;
    assert_eq!(first["type"], "lmgw.session.bind_pending", "{first}");
    // 240 000 bytes more would pass the budget beside what was kept; once
    // the kept audio went, the event is only refused as any is while
    // pending.
    p.send(json!({"type": "pad", "pad": "p".repeat(240_000)}));
    let refused = p.next_but_states().await;
    assert_eq!(refused["error"]["code"], "bind_pending", "{refused}");
    // The call waits on: take_over binds it.
    p.send(json!({"type": "lmgw.session.take_over"}));
    let created = p.next_but_states().await;
    assert_eq!(created["type"], "session.created", "{created}");
}
