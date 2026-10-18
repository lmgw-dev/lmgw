//! Liveness and closes of WebRTC calls (realtime calls design §8, §13.1;
//! WP5), end to end over loopback UDP with str0m as the phone, as
//! `realtime_calls` drives it:
//!
//! - `liveness` (§8.1): a call's session reads the phone's traffic and its
//!   acknowledgements as two facts. It stays up past two intervals while the
//!   phone sends and reads, ends dead one interval after the phone went
//!   silent, and stalled one interval after the phone, still sending its
//!   microphone, stopped acknowledging; a call that never connects, or whose
//!   `oai-events` never opens, ends one interval on. A dead or stalled
//!   call's `lmgw.session.closed` is handed to a writer that takes it at
//!   once (`say_closed_now`), ahead of the close.
//! - `kinds` (§8.2, §8.4): for each kind of a running bound session's end a
//!   call on the plain listener can meet, the phone's last event is
//!   `lmgw.session.closed` with its kind, `by` and message, read by
//!   `lmgw-client` as its `CloseKind`, and a paired device's feed says what
//!   §8.2's last column says — `voice.ended` with `taken_over_by` for a
//!   takeover, `closed` for a plain end, `revoked` for the caller's key
//!   revoked, the feed's end for a stop. Too big among them: what waits for
//!   the session past `realtime.max_message_mb`, behind a phone that reads
//!   nothing, ends with `closed` naming the setting, last.
//! - `end` (§8.1's End, §8.2): the last event is delivered before the close
//!   (its first transmission lost, its retransmission still comes before the
//!   close), at most one interval after the close began, past which the call
//!   is closed without it; at 0 the call stays closing, shown as such, and a
//!   hangup closes it at once. The phone's own close applies what came
//!   before it (a stop and a push-to-talk commit, §13.1's End), ends the
//!   call as a hangup does and sends nothing after it.
//!
//! Every call is an owner key's, on the plain listener: a paired device
//! reaches lmgw through the device listener, whose calls are calls WP7's. So
//! what only a device's own call meets — its thread out of its reach, and
//! its own feed's view of its call's end (§8.4's `revoked` and the feed's
//! end, the thread's removal) — is tested with WP7; the session's close site
//! and the call's close after delivery it takes are the ones tested here. A
//! paired device appears as a reader of its feed.
//!
//! The bounds are measured on a paused clock: the phone connects in real
//! time, then the test pauses tokio's clock, which lmgw and the phone share
//! (`realtime_calls_net::client::now`), and the timeline is virtual from
//! there: a bound is met exactly, and a test waits no interval out.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::Instant;

use lmgw_core::realtime::calls::CallState;

use crate::chat_feed::{Feed, Frame};
use crate::common::patience::until;
use crate::device_chat::{chat_thread, pair, Device};
use crate::realtime_calls::{calls_world, key_client, Phone};
use crate::realtime_chat_thread::World;

mod end;
mod kinds;
mod liveness;

/// An Opus frame of 20 ms of silence (RFC 6716's TOC byte for CELT
/// fullband 20 ms, one frame, and its two-byte silence payload): what a
/// muted microphone's track keeps sending.
const SILENCE: &[u8] = &[0xF8, 0xFF, 0xFE];

/// An owner key's call bound to a thread, its session running, and a
/// paired device's feed open from before the call (module doc).
struct BoundCall {
    w: World,
    /// The calling key's id (owner key `phone`).
    key: i64,
    tid: i64,
    phone: Phone,
    /// Device `tablet`, whose feed [`Self::feed`] is.
    device: Device,
    feed: Feed,
}

impl BoundCall {
    /// On a calls world `tweak`ed: device `tablet`'s feed open, then owner
    /// key `phone`'s call bound to a thread and past `session.created`.
    async fn start(tweak: impl FnOnce(&mut lmgw_core::config::Settings)) -> Self {
        let w = calls_world(tweak).await;
        let device = pair(&w, "tablet", json!({})).await;
        let feed = Feed::open(&w, &device.client, "", None).await;
        let tid = w.thread("chatty", json!({})).await;
        let (key, caller) = key_client(&w, "owner", "phone", json!({})).await;
        let mut phone = Phone::connected(&w, &caller, &format!("chat_thread={tid}")).await;
        let created = phone.next_but_states().await;
        assert_eq!(created["type"], "session.created", "{created}");
        Self {
            w,
            key,
            tid,
            phone,
            device,
            feed,
        }
    }

    /// Drive the phone until nothing lmgw wrote on the call waits for its
    /// acknowledgement: a quiet start for a test that measures from here.
    async fn settled(&mut self) {
        loop {
            let info = self.w.state.calls.get(&self.phone.id).expect("live");
            if info.unacked_since.is_none() {
                return;
            }
            self.phone.c.run_for(Duration::from_millis(20)).await;
        }
    }

    /// What the device's feed said of the thread from the call's
    /// `voice.bound` on: read until its `voice.ended` — which the session
    /// publishes as its binding lets go, and that may come after the call's
    /// close reached the phone — then past a thread the owner makes, so a
    /// second one would be read too; or to the feed's end.
    async fn feed_after(&mut self) -> Vec<Frame> {
        let tid = self.tid;
        self.feed
            .until(10, |f| {
                f.iter()
                    .any(|f| f.event == "voice.ended" && f.data["thread_id"] == tid)
            })
            .await;
        if !self.feed.ended {
            let marker = chat_thread(&self.w, &self.w.gw.client(), "chatty").await;
            self.feed
                .until(10, |f| {
                    f.iter()
                        .any(|f| f.event == "thread.created" && f.data["id"] == marker)
                })
                .await;
        }
        let names = |f: &Frame| f.data["thread_id"] == tid || f.data["id"] == tid;
        let from = self
            .feed
            .frames
            .iter()
            .position(|f| f.event == "voice.bound" && names(f))
            .unwrap_or_else(|| panic!("the device heard the call's bind: {:#?}", self.feed.frames));
        self.feed.frames[from..]
            .iter()
            .filter(|f| names(f))
            .cloned()
            .collect()
    }
}

/// The `voice.ended` frames among `frames`.
fn voice_ended(frames: &[Frame]) -> Vec<Value> {
    frames
        .iter()
        .filter(|f| f.event == "voice.ended")
        .map(|f| f.data.clone())
        .collect()
}

/// The `lmgw.session.closed` that ends `events`, which must be the last,
/// and what `lmgw-client` reads of it.
fn closed_last(events: &[Value]) -> (&Value, lmgw_client::realtime::CallClose) {
    let last = events
        .last()
        .unwrap_or_else(|| panic!("no event before the close"));
    assert_eq!(last["type"], "lmgw.session.closed", "{events:#?}");
    let read: lmgw_api_types::realtime::SessionClosed =
        serde_json::from_value(last.clone()).unwrap();
    (last, lmgw_client::realtime::call_close(&read))
}

/// Wait, on the paused clock, until call `id` has let go — closing, or
/// gone: when it did.
async fn let_go_at(w: &World, id: &str) -> Instant {
    until("the call let go", || {
        w.state
            .calls
            .get(id)
            .is_none_or(|i| i.state == CallState::Closing)
    })
    .await;
    Instant::now()
}

/// Wait, on the paused clock, until call `id` is gone from the registry,
/// its `Rtc` closed: when it was.
async fn removed_at(w: &World, id: &str) -> Instant {
    until("the call removed", || w.state.calls.get(id).is_none()).await;
    Instant::now()
}

/// `got` lies within `[bound, bound + POLL_SLACK]`: met at the bound, on the
/// paused clock, give or take the polls that saw it.
fn at_the_bound(what: &str, got: Duration, bound: Duration) {
    assert!(
        got >= bound && got <= bound + POLL_SLACK,
        "{what}: after {got:?}, the bound being {bound:?}"
    );
}

/// What a wait on the paused clock may add to a bound: the poll's own
/// period (`patience`'s 10 ms) and the turns of the tasks that act on it,
/// virtual time all, so no figure of this machine's speed.
const POLL_SLACK: Duration = Duration::from_millis(100);
