//! Whether a pairing token is pending (device link design §4.2, D11): what
//! every device listener handshake asks. While one is, a client may come
//! without a certificate (for enrolment alone); while none is, the handshake
//! requires one, so the surface without authentication lives only as long
//! as a token does.
//!
//! "Pending" is a token alive: minted, not lapsed, not ended. A used one
//! stays pending until then, so the key that used it can ask for its answer
//! again (§3.3's note of 2026-10-15), or until its device makes a request
//! with the certificate that answer carried (D8's note of 2026-10-15). The
//! tokens themselves are the store's (`devices::tokens`): each write that
//! mints, uses or ends one says so here as it commits ([`PendingTokens::set`],
//! [`PendingTokens::answered`], [`PendingTokens::end`]), and a start and the
//! CA's rotation load the store's whole set ([`PendingTokens::replace`]).

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};

/// The pending pairing tokens, by device row (one token per row, §3.3).
#[derive(Debug, Default)]
pub struct PendingTokens(Mutex<HashMap<i64, Pending>>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    expires_at: DateTime<Utc>,
    /// The serial of the certificate the token's enrolment answered, once
    /// it was used.
    answered: Option<String>,
}

impl PendingTokens {
    /// Device `key_id` has a new token, pending until `expires_at` (it
    /// replaces the row's earlier one).
    pub fn set(&self, key_id: i64, expires_at: DateTime<Utc>) {
        self.lock().insert(
            key_id,
            Pending {
                expires_at,
                answered: None,
            },
        );
    }

    /// Device `key_id`'s token was used, and its answer carried the
    /// certificate `serial`.
    pub fn answered(&self, key_id: i64, serial: &str) {
        if let Some(p) = self.lock().get_mut(&key_id) {
            p.answered = Some(serial.to_string());
        }
    }

    /// Device `key_id`'s token ended.
    pub fn end(&self, key_id: i64) {
        self.lock().remove(&key_id);
    }

    /// Device `key_id`'s token ends here if its enrolment answered the
    /// certificate `serial`: its device presents that certificate, so it
    /// holds its answer. Whether it did; once only.
    pub fn take_answered(&self, key_id: i64, serial: &str) -> bool {
        let mut tokens = self.lock();
        let hit = tokens
            .get(&key_id)
            .is_some_and(|p| p.answered.as_deref() == Some(serial));
        if hit {
            tokens.remove(&key_id);
        }
        hit
    }

    /// Exactly `alive` from now on: the store's tokens alive, each with its
    /// device, its expiry and the serial its enrolment answered.
    pub fn replace(&self, alive: impl IntoIterator<Item = (i64, DateTime<Utc>, Option<String>)>) {
        *self.lock() = alive
            .into_iter()
            .map(|(id, expires_at, answered)| {
                (
                    id,
                    Pending {
                        expires_at,
                        answered,
                    },
                )
            })
            .collect();
    }

    /// Whether any token is pending at `now`; the expired ones are
    /// forgotten.
    pub fn any(&self, now: DateTime<Utc>) -> bool {
        self.count(now) > 0
    }

    /// How many tokens are pending at `now`; the expired ones are
    /// forgotten.
    pub fn count(&self, now: DateTime<Utc>) -> usize {
        let mut tokens = self.lock();
        tokens.retain(|_, p| p.expires_at > now);
        tokens.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Pending>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_pending_until_it_expires_or_ends() {
        let now = Utc::now();
        let t = PendingTokens::default();
        assert!(!t.any(now));
        t.set(1, now + chrono::Duration::minutes(10));
        t.set(2, now + chrono::Duration::seconds(1));
        assert!(t.any(now));
        t.end(1);
        assert!(t.any(now), "2 is still pending");
        assert!(!t.any(now + chrono::Duration::seconds(2)), "2 expired");
        t.replace([(3, now + chrono::Duration::minutes(1), None)]);
        assert_eq!(t.count(now), 1);
        t.replace([]);
        assert!(!t.any(now));
    }

    /// D8's note of 2026-10-15: a used token ends at its own certificate,
    /// once; another certificate of the device, or a token minted since,
    /// leaves it.
    #[test]
    fn a_used_token_ends_at_the_certificate_it_answered() {
        let later = Utc::now() + chrono::Duration::minutes(10);
        let t = PendingTokens::default();
        t.set(1, later);
        assert!(!t.take_answered(1, "0a"), "unused");
        t.answered(1, "0a");
        assert!(!t.take_answered(1, "0b"), "another certificate");
        assert!(t.take_answered(1, "0a"));
        assert!(!t.take_answered(1, "0a"), "once");
        assert!(!t.any(Utc::now()));
        // A new token replaces the used one before its device asked.
        t.replace([(2, later, Some("0c".into()))]);
        t.set(2, later);
        assert!(!t.take_answered(2, "0c"));
        assert!(t.any(Utc::now()));
    }
}
