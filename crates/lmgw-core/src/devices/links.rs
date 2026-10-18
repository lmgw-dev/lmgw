//! A device's open long-lived connections (client-apps design §1.6, L15).
//!
//! Counted per key and kind while they live, so the Devices card can say
//! "online (feed, voice, tools)" without a query; `last_seen_at` is stamped
//! as each opens and as each closes, so "last seen" is right for a device
//! that is not connected now.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, Utc};

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

/// What one open connection tells the card (device link design §9.3):
/// "feed (keep-alive 120 s, since 09:00, from 192.0.2.2)".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkDetail {
    pub kind: LinkKind,
    /// When it opened.
    pub since: DateTime<Utc>,
    /// The TCP peer it came from; `None` for a connection that carries no
    /// connect info (a context built by hand).
    pub peer: Option<SocketAddr>,
    /// The keep-alive it asked for, in seconds; only a feed has one.
    pub keepalive_s: Option<u32>,
    /// The WebRTC call a voice link is (`rtc_…`, calls design §9); `None`
    /// for a WebSocket session and for the other kinds.
    pub call: Option<std::sync::Arc<str>>,
}

/// The connections every device key has open, each with its details.
#[derive(Debug, Default)]
pub struct Links {
    open: Mutex<HashMap<i64, Vec<(u64, LinkDetail)>>>,
    next: AtomicU64,
}

impl Links {
    /// The kinds `key_id` has open now, in [`LinkKind::ALL`]'s order; empty
    /// for a device that is offline (and for every key that is no device).
    pub fn online(&self, key_id: i64) -> Vec<&'static str> {
        self.counts(key_id).into_iter().map(|(k, _)| k).collect()
    }

    /// How many of each kind `key_id` has open now, the kinds as
    /// [`Self::online`] lists them (review W4-25: a reconnect pile-up is
    /// seen on the card).
    pub fn counts(&self, key_id: i64) -> Vec<(&'static str, u32)> {
        let open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let Some(links) = open.get(&key_id) else {
            return Vec::new();
        };
        LinkKind::ALL
            .into_iter()
            .filter_map(|k| {
                let n = links.iter().filter(|(_, d)| d.kind == k).count();
                (n > 0).then(|| (k.as_str(), n as u32))
            })
            .collect()
    }

    /// Every connection `key_id` has open, in [`LinkKind::ALL`]'s order and
    /// the oldest of a kind first.
    pub fn details(&self, key_id: i64) -> Vec<LinkDetail> {
        let open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<LinkDetail> = open
            .get(&key_id)
            .map(|links| links.iter().map(|(_, d)| d.clone()).collect())
            .unwrap_or_default();
        out.sort_by_key(|d| (d.kind.slot(), d.since));
        out
    }

    fn add(&self, key_id: i64, detail: LinkDetail) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        open.entry(key_id).or_default().push((id, detail));
        id
    }

    fn remove(&self, key_id: i64, id: u64) {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(links) = open.get_mut(&key_id) {
            links.retain(|(i, _)| *i != id);
            if links.is_empty() {
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
    /// Its entry in [`Links`].
    id: u64,
    /// Stamp `last_seen_at` as it closes. Not for a link closed by a Rotate
    /// (review W2-19): the old key's last moment is not the device being
    /// seen, and "last seen just now" would read as the new pairing working.
    stamp_close: bool,
}

impl LinkGuard {
    pub(super) async fn open(state: &SharedState, key_id: i64, detail: LinkDetail) -> Self {
        let id = state.devices.links.add(key_id, detail);
        stamp(state, key_id).await;
        state.telemetry.keys(key_id, "link");
        Self {
            state: state.clone(),
            key_id,
            id,
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
        self.state.devices.links.remove(self.key_id, self.id);
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
