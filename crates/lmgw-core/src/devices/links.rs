//! A device's open long-lived connections (client-apps design §1.6, L15).
//!
//! Counted per key and kind while they live, so the Devices card can say
//! "online (feed, voice, tools)" without a query; `last_seen_at` is stamped
//! as each opens and as each closes, so "last seen" is right for a device
//! that is not connected now.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::state::SharedState;

/// Which kind of long-lived connection a device holds open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkKind {
    /// The Chat change feed (`GET /chat/api/feed`, §2).
    Feed,
    /// A realtime session (`/v1/realtime`), bound to a thread or not.
    Voice,
    /// The device's MCP host link (`GET /mcp/host`, §5).
    Tools,
}

impl LinkKind {
    /// The word the card prints; also the order it prints them in.
    pub const ALL: [LinkKind; 3] = [Self::Feed, Self::Voice, Self::Tools];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Feed => "feed",
            Self::Voice => "voice",
            Self::Tools => "tools",
        }
    }

    fn slot(self) -> usize {
        match self {
            Self::Feed => 0,
            Self::Voice => 1,
            Self::Tools => 2,
        }
    }
}

/// How many connections of each kind every device key has open.
#[derive(Debug, Default)]
pub struct Links {
    open: Mutex<HashMap<i64, [u32; 3]>>,
}

impl Links {
    /// The kinds `key_id` has open now, in [`LinkKind::ALL`]'s order; empty
    /// for a device that is offline (and for every key that is no device).
    pub fn online(&self, key_id: i64) -> Vec<&'static str> {
        let open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let Some(counts) = open.get(&key_id) else {
            return Vec::new();
        };
        LinkKind::ALL
            .into_iter()
            .filter(|k| counts[k.slot()] > 0)
            .map(LinkKind::as_str)
            .collect()
    }

    /// How many of each kind `key_id` has open now, the kinds as
    /// [`Self::online`] lists them (review W4-25: a reconnect pile-up is
    /// seen on the card).
    pub fn counts(&self, key_id: i64) -> Vec<(&'static str, u32)> {
        let open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let Some(counts) = open.get(&key_id) else {
            return Vec::new();
        };
        LinkKind::ALL
            .into_iter()
            .filter(|k| counts[k.slot()] > 0)
            .map(|k| (k.as_str(), counts[k.slot()]))
            .collect()
    }

    fn add(&self, key_id: i64, kind: LinkKind) {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        open.entry(key_id).or_default()[kind.slot()] += 1;
    }

    fn remove(&self, key_id: i64, kind: LinkKind) {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(counts) = open.get_mut(&key_id) {
            counts[kind.slot()] = counts[kind.slot()].saturating_sub(1);
            if counts.iter().all(|c| *c == 0) {
                open.remove(&key_id);
            }
        }
    }
}

/// One open connection: counted while it lives, `last_seen_at` stamped as it
/// opens and as it is dropped.
pub struct LinkGuard {
    state: SharedState,
    key_id: i64,
    kind: LinkKind,
    /// Stamp `last_seen_at` as it closes. Not for a link closed by a Rotate
    /// (review W2-19): the old key's last moment is not the device being
    /// seen, and "last seen just now" would read as the new pairing working.
    stamp_close: bool,
}

impl LinkGuard {
    pub(super) async fn open(state: &SharedState, key_id: i64, kind: LinkKind) -> Self {
        state.devices.links.add(key_id, kind);
        stamp(state, key_id).await;
        state.telemetry.keys(key_id, "link");
        Self {
            state: state.clone(),
            key_id,
            kind,
            stamp_close: true,
        }
    }

    /// Close without stamping `last_seen_at` ([`Self::stamp_close`]).
    pub(super) fn close_unseen(&mut self) {
        self.stamp_close = false;
    }
}

impl Drop for LinkGuard {
    fn drop(&mut self) {
        self.state.devices.links.remove(self.key_id, self.kind);
        // A drop cannot await: the close's stamp is written on the runtime
        // the connection ran on. Outside one (a test tearing down) there is
        // nothing left to write for.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let state = self.state.clone();
            let id = self.key_id;
            let stamped = self.stamp_close;
            rt.spawn(async move {
                if stamped {
                    stamp(&state, id).await;
                }
                // Said once the stamp is in: the Keys page refetches on it
                // and reads "last seen" from the table (`/api/events`).
                state.telemetry.keys(id, "link");
            });
        }
    }
}

/// `last_seen_at = now`. A failed write is logged, never a refusal: the
/// connection is not the stamp's to end.
async fn stamp(state: &SharedState, key_id: i64) {
    if let Err(e) = crate::store::touch_key_last_seen(&state.db, key_id).await {
        tracing::warn!("device key {key_id}: last_seen_at not written: {e}");
    }
}
