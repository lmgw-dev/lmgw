//! The device socket group (calls design §3.1, WP7): one UDP socket per
//! address the device listener binds (the loopback pair, `[::]`, or one
//! address), on its port, following it live.
//!
//! The device listener publishes where it serves at every start, move and
//! stop (`DeviceListener::watch`). One follower per gateway reads it and
//! applies each change in order: the group's sockets close as a stop closes
//! them — the owners of its calls told ([`Ending::ShuttingDown`]), room for
//! a bound call's last event (`lmgw.session.closed {kind:
//! "shutting_down"}`, §8.2), the rest closed by force once
//! [`STOP_WITHIN`](crate::server::STOP_WITHIN) has passed — and then the
//! new ones bind ([`Calls::bind_group`]). Only device calls end; the plain
//! group and its calls are untouched (device link §4.4, §8).
//!
//! - **Started by the device listener's arm** (`DeviceListener::claim`),
//!   once per gateway, so a gateway and a suite that arms the listener
//!   alone get the same group. It lives while the listener does: the watch
//!   closing ends it.
//! - **One follower, in order**: the old sockets are closed before the new
//!   ones bind, so a move onto an overlapping address and port (`loopback`
//!   to `all` on one port) never meets its own old socket, and a restart
//!   needs no release wait for this group, as the device listener's TCP
//!   side needs none (its next arm stops what the last one left). The
//!   latest state is read after the close, so a change that came while it
//!   waited for its calls is bound, not the one the cycle began on.
//! - **The gap**: from the cycle's start to the new bind the group reads
//!   [`GroupState::Moving`] (a device call is refused 503 with
//!   `Retry-After`: at most [`STOP_WITHIN`](crate::server::STOP_WITHIN)),
//!   and the group records the start it is bound for
//!   (`Bound::served_at`): a POST on a connection of another start is
//!   refused the same way, so an in-flight POST of the old start is never
//!   answered on the new group's socket.
//! - **A failed bind** stays failed until the listener changes, or a save
//!   of the device settings with no change wakes the follower again
//!   (`DeviceListener::reconcile`): the close is a no-op and the bind is
//!   tried again.
//! - **Off** (`realtime.webrtc_enabled` as the gateway's start read it,
//!   [`Calls::serve_plain`]): the group is [`GroupState::Off`] and binds
//!   nothing while the device listener runs; a device call is refused 503
//!   naming the setting.
//!
//! [`Ending::ShuttingDown`]: super::Ending::ShuttingDown

use std::sync::atomic::Ordering;

use tokio::sync::watch;

use super::{Calls, Group, GroupState};
use crate::server::device_listener::Bound;

impl Calls {
    /// Follow the device listener from now on (module doc): bind the device
    /// group where it serves, move it as it moves, close it as it stops. A
    /// second call while the follower runs does nothing.
    pub fn follow_devices(&self, listener: watch::Receiver<Option<Bound>>) {
        if self.inner.following.swap(true, Ordering::AcqRel) {
            return;
        }
        let following = Following(self.clone());
        tokio::spawn(async move {
            follow(&following.0, listener).await;
            drop(following);
        });
    }

    /// Settle the device group on the device listener's latest state
    /// (`bound`), after its old sockets are closed: bind where it serves,
    /// or leave the group off or down.
    async fn settle(&self, bound: Option<Bound>) {
        let off = self.inner.off_at_start.load(Ordering::Relaxed);
        match bound {
            Some(b) if !off => {
                self.bind_group(Group::Devices, &b.addrs, Some(b.served_at))
                    .await;
            }
            bound => {
                if off && bound.is_some() {
                    tracing::info!(
                        "WebRTC calls on the device listener are off (realtime.webrtc_enabled)"
                    );
                }
                let state = if off {
                    GroupState::Off
                } else {
                    GroupState::Down
                };
                self.set_group(Group::Devices, state, Vec::new(), None);
            }
        }
    }
}

/// The follower is between a change and the new bind while this lives: the
/// device group reads [`GroupState::Moving`].
struct Moving<'a>(&'a Calls);

impl<'a> Moving<'a> {
    fn begin(calls: &'a Calls) -> Self {
        calls.inner.moving.store(true, Ordering::Release);
        Self(calls)
    }
}

impl Drop for Moving<'_> {
    fn drop(&mut self) {
        self.0.inner.moving.store(false, Ordering::Release);
    }
}

/// The follower's mark: let go when its task ends, a panic included, so the
/// next arm starts it again.
struct Following(Calls);

impl Drop for Following {
    fn drop(&mut self) {
        self.0.inner.following.store(false, Ordering::Release);
    }
}

/// Apply every state the device listener publishes, in order, until it is
/// gone. Each cycle closes the group first, then reads the latest state and
/// binds that: a change that came while the close waited for its calls is
/// bound, not the one the cycle began on (N1). Also what a save of the same
/// settings does to a failed bind: it wakes this with no change, the close
/// is a no-op, and the bind is tried again.
async fn follow(calls: &Calls, mut listener: watch::Receiver<Option<Bound>>) {
    let mut first = true;
    loop {
        if !std::mem::take(&mut first) && listener.changed().await.is_err() {
            return;
        }
        let moving = Moving::begin(calls);
        let was_bound = !calls.socket_ids(Group::Devices).is_empty();
        calls.close_group(Group::Devices).await;
        if was_bound {
            tracing::info!("WebRTC calls on the device listener: its UDP sockets closed");
        }
        let latest = listener.borrow_and_update().clone();
        calls.settle(latest).await;
        drop(moving);
    }
}
