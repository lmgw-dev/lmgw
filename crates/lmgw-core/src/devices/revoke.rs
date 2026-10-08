//! The revocation signal (client-apps design §1.6, L17, L18).
//!
//! **Revocation is said, not implied.** A device's long-lived connections —
//! its realtime sessions today, its Chat feed and its MCP host link once they
//! exist — each watch one signal keyed by key id, and end the moment it is
//! raised: the feed with `event: revoked`, the WebSocket links with the close
//! 4003, both naming which of the four happened.
//!
//! - **Disable, Rotate and Delete** raise it from the op that did it, after
//!   the snapshot is reloaded — so a client that reconnects at once is
//!   refused by the gate rather than let in again.
//! - **Expiry** raises itself: each watch sleeps until its key's `expires_at`
//!   as the snapshot says it now, and wakes to re-read it whenever any key's
//!   signal or expiry changes ([`Revocations::rearm`]). A watch is the only
//!   party that needs to know, so there is no timer task to keep alive.
//!
//! Lossless by construction: a generation counter on a `watch` channel plus
//! the last revocation per key, rather than a broadcast a slow watcher could
//! lag on and miss.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::watch;

use crate::state::AppState;

/// Why a device's connections ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    Disabled,
    Rotated,
    Deleted,
    Expired,
}

impl RevokeReason {
    /// The machine word: `voice.ended`'s and `revoked`'s `reason`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Rotated => "rotated",
            Self::Deleted => "deleted",
            Self::Expired => "expired",
        }
    }

    /// What a client does about it (`RevokeKind`), for a paired device's
    /// key (`device`) or another kind's: the token a 4003 close's reason
    /// starts with and the feed's `revoked` carries as `kind`. A device's
    /// Rotate and Delete are one kind, `key_unknown`: either way it pairs
    /// again, as its next request's `401 device_key_unknown` says.
    pub fn kind(self, device: bool) -> lmgw_api_types::realtime::RevokeKind {
        use lmgw_api_types::realtime::RevokeKind;
        if !device {
            return RevokeKind::Revoked;
        }
        match self {
            Self::Disabled => RevokeKind::DeviceDisabled,
            Self::Expired => RevokeKind::KeyExpired,
            Self::Rotated | Self::Deleted => RevokeKind::KeyUnknown,
        }
    }

    /// The sentence a close and the feed's `revoked` carry, naming the key
    /// — `who` is `devices::who`'s "device 'phone'" or "key 'laptop'" — and
    /// what happened (a close cuts it to fit, `devices::close_reason`). The
    /// plain reason: the `revoked` event and the 4003 close say that it is
    /// a revocation, so a client that shows "revoked (…)" does not say it
    /// twice.
    pub fn message(self, who: &str) -> String {
        let device = who.starts_with("device '");
        match self {
            Self::Disabled => format!("{who} was disabled"),
            Self::Rotated if device => format!("{who} was rotated — pair it again"),
            Self::Rotated => format!("{who} was rotated"),
            Self::Deleted => format!("{who} was deleted"),
            Self::Expired => format!("{who} expired"),
        }
    }
}

/// The signal: a generation that moves on every change, and the last
/// revocation of each key with the generation it was raised at.
#[derive(Debug)]
pub struct Revocations {
    generation: watch::Sender<u64>,
    fired: Mutex<HashMap<i64, (u64, RevokeReason)>>,
}

impl Default for Revocations {
    fn default() -> Self {
        Self {
            generation: watch::Sender::new(0),
            fired: Mutex::default(),
        }
    }
}

impl Revocations {
    /// The generation now: what a principal resolved at this moment has
    /// already seen (`RequestCtx::revocation_mark`).
    pub fn mark(&self) -> u64 {
        *self.generation.borrow()
    }

    /// End every connection `key_id` has open, for `reason`.
    pub fn revoke(&self, key_id: i64, reason: RevokeReason) {
        let mut fired = self.fired.lock().unwrap_or_else(|e| e.into_inner());
        let mut at = 0;
        self.generation.send_modify(|g| {
            *g += 1;
            at = *g;
        });
        fired.insert(key_id, (at, reason));
        tracing::info!("key {key_id}: revoked ({})", reason.as_str());
    }

    /// A key's expiry changed: every watch re-reads its own.
    pub fn rearm(&self) {
        self.generation.send_modify(|g| *g += 1);
    }

    /// A watch on `key_id` that counts revocations raised after generation
    /// `mark`, the one its principal was resolved at, for the credential
    /// `fingerprint` it presented (`ApiKey::fingerprint`): a row that holds
    /// another one now was rotated, whatever the signal saw.
    pub fn watch_from(&self, key_id: i64, mark: u64, fingerprint: String) -> RevocationWatch {
        RevocationWatch {
            key_id,
            mark,
            fingerprint,
            rx: self.generation.subscribe(),
        }
    }

    fn fired_after(&self, key_id: i64, mark: u64) -> Option<RevokeReason> {
        let fired = self.fired.lock().unwrap_or_else(|e| e.into_inner());
        fired
            .get(&key_id)
            .filter(|(at, _)| *at > mark)
            .map(|(_, r)| *r)
    }
}

/// One connection's view of the signal.
pub struct RevocationWatch {
    key_id: i64,
    mark: u64,
    /// The credential the principal was resolved with.
    fingerprint: String,
    rx: watch::Receiver<u64>,
}

/// How long a watch sleeps before it looks at the wall clock again — a
/// re-check interval, not a limit. A tokio sleep is monotonic: it does not
/// advance across a suspend, and a wall-clock step does not move it, so a
/// watch that slept toward a far expiry in one go could close late by as
/// much as it slept (review W2-7). Short enough that an expiry is honoured
/// within this much of it whatever the clock did; a watch awaited as one
/// long-lived future (the feed, the host link) needs nothing more.
const RECHECK: Duration = Duration::from_secs(30);

impl RevocationWatch {
    pub fn key_id(&self) -> i64 {
        self.key_id
    }

    /// Resolves with the first revocation of this key after the mark.
    pub async fn revoked(&mut self, state: &AppState) -> RevokeReason {
        loop {
            // Mark the generation seen before reading, so a change between
            // the read and the wait below wakes the wait.
            self.rx.borrow_and_update();
            if let Some(r) = state
                .devices
                .revocations
                .fired_after(self.key_id, self.mark)
            {
                return r;
            }
            let snap = state.snapshot();
            let Some(key) = snap.api_keys.iter().find(|k| k.id == self.key_id) else {
                // Gone without a signal (a path that deleted the row directly):
                // the credential this connection presented is gone either way.
                return RevokeReason::Deleted;
            };
            if !key.enabled {
                return RevokeReason::Disabled;
            }
            // Rotated, whether or not this watch saw the signal: one raised
            // before the mark it was resolved with — a Rotate racing the
            // request's resolution — is caught here (review W2-5).
            if key.fingerprint() != self.fingerprint {
                return RevokeReason::Rotated;
            }
            let now = chrono::Utc::now();
            let until = match key
                .policy
                .expires_at
                .as_deref()
                .and_then(crate::policy::expiry_deadline)
            {
                Some(at) if at <= now => return RevokeReason::Expired,
                Some(at) => (at - now).to_std().unwrap_or(Duration::ZERO).min(RECHECK),
                None => RECHECK,
            };
            tokio::select! {
                changed = self.rx.changed() => {
                    if changed.is_err() {
                        // The gateway is going away and nothing will raise
                        // the signal again; an expiry still comes by the
                        // clock (review W2-7).
                        tokio::time::sleep(until).await;
                    }
                }
                () = tokio::time::sleep(until) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! The signal's own rules (review W2-12): the mark, the snapshot's
    //! fallbacks for a change no op named, and the expiry by the clock.

    use std::time::Duration;

    use super::RevokeReason;
    use crate::state::{AppState, SharedState};

    /// A gateway with one client key `k`: its id and fingerprint.
    async fn with_key() -> (SharedState, i64, String) {
        let state = AppState::init_for_tests().await.unwrap();
        sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES ('k', ?1, 1)")
            .bind(crate::config::hash_api_key("lmgw-k"))
            .execute(&state.db)
            .await
            .unwrap();
        state.reload_snapshot().await.unwrap();
        let snap = state.snapshot();
        let key = snap.api_keys.iter().find(|k| k.name == "k").unwrap();
        (state.clone(), key.id, key.fingerprint())
    }

    /// The reason the watch resolves with within `ms`, if it does.
    async fn within(
        state: &SharedState,
        w: &mut super::RevocationWatch,
        ms: u64,
    ) -> Option<RevokeReason> {
        tokio::time::timeout(Duration::from_millis(ms), w.revoked(state))
            .await
            .ok()
    }

    #[tokio::test]
    async fn only_a_revocation_after_the_mark_counts() {
        let (state, id, fp) = with_key().await;
        let r = &state.devices.revocations;
        r.revoke(id, RevokeReason::Disabled);
        // Resolved after that one (re-enabled since): it does not count.
        let mut w = r.watch_from(id, r.mark(), fp);
        assert_eq!(within(&state, &mut w, 50).await, None);
        r.revoke(id, RevokeReason::Rotated);
        assert_eq!(
            within(&state, &mut w, 500).await,
            Some(RevokeReason::Rotated)
        );
        // Another key's revocation is not this one's.
        let (state2, id2, fp2) = with_key().await;
        let r2 = &state2.devices.revocations;
        let mut w2 = r2.watch_from(id2, r2.mark(), fp2);
        r2.revoke(id2 + 1, RevokeReason::Deleted);
        assert_eq!(within(&state2, &mut w2, 50).await, None);
    }

    #[tokio::test]
    async fn a_change_no_op_named_is_read_from_the_snapshot() {
        // Disabled by hand: the reload re-arms the watch, which reads it.
        let (state, id, fp) = with_key().await;
        let r = &state.devices.revocations;
        let mut w = r.watch_from(id, r.mark(), fp.clone());
        sqlx::query("UPDATE api_keys SET enabled = 0 WHERE id = ?1")
            .bind(id)
            .execute(&state.db)
            .await
            .unwrap();
        state.reload_snapshot().await.unwrap();
        assert_eq!(
            within(&state, &mut w, 500).await,
            Some(RevokeReason::Disabled)
        );

        // Rotated: the same row, another credential — and a watch resolved
        // after the signal it missed (W2-5's race) still sees it.
        let (state, id, fp) = with_key().await;
        sqlx::query("UPDATE api_keys SET key_hash = 'other' WHERE id = ?1")
            .bind(id)
            .execute(&state.db)
            .await
            .unwrap();
        state.reload_snapshot().await.unwrap();
        let r = &state.devices.revocations;
        let mut w = r.watch_from(id, r.mark(), fp);
        assert_eq!(
            within(&state, &mut w, 500).await,
            Some(RevokeReason::Rotated)
        );

        // Deleted.
        let (state, id, fp) = with_key().await;
        let r = &state.devices.revocations;
        let mut w = r.watch_from(id, r.mark(), fp);
        sqlx::query("DELETE FROM api_keys WHERE id = ?1")
            .bind(id)
            .execute(&state.db)
            .await
            .unwrap();
        state.reload_snapshot().await.unwrap();
        assert_eq!(
            within(&state, &mut w, 500).await,
            Some(RevokeReason::Deleted)
        );
    }

    #[tokio::test]
    async fn an_expiry_set_before_the_watch_comes_by_the_clock() {
        let (state, id, fp) = with_key().await;
        let at = (chrono::Utc::now() + chrono::Duration::milliseconds(700)).to_rfc3339();
        sqlx::query("UPDATE api_keys SET expires_at = ?2 WHERE id = ?1")
            .bind(id)
            .bind(&at)
            .execute(&state.db)
            .await
            .unwrap();
        state.reload_snapshot().await.unwrap();
        let r = &state.devices.revocations;
        let mut w = r.watch_from(id, r.mark(), fp.clone());
        assert_eq!(within(&state, &mut w, 200).await, None, "not yet");
        assert_eq!(
            within(&state, &mut w, 2_000).await,
            Some(RevokeReason::Expired)
        );
        // One already past resolves at once.
        let mut late = r.watch_from(id, r.mark(), fp);
        assert_eq!(
            within(&state, &mut late, 100).await,
            Some(RevokeReason::Expired)
        );
    }
}
