//! The pairing links this process minted and has not handed off yet
//! (client-apps design §1.4, review W2-1; device link design §3.1).
//!
//! lmgw's own window hands an `lmgw-pair:` link to the desktop — which
//! starts whichever client is registered for the scheme — because WebKitGTK
//! launches no scheme handler itself. The same window hosts the Chat's HTML
//! preview, whose scripts can navigate their frame, and the navigation
//! handler cannot tell a frame from the page. A device holds `Chat`, so "a
//! link that looks like a pairing link" would let any device put a link of
//! its choosing in front of the owner.
//!
//! So the shell hands off only an **exact** link that `key_create` or
//! `device_token` minted here, **once**, and while its token lives: a link
//! replayed from a preview can then only be the owner's own fresh one. A
//! link is forgotten when its token ends (a new token, the QR card's close,
//! a revocation, the CA's rotation) and refused once its token lapses.

use std::sync::Mutex;

use chrono::{DateTime, Utc};

/// The minted, not yet handed-off links, with the device each pairs and
/// when its token lapses.
#[derive(Debug, Default)]
pub struct MintedLinks {
    open: Mutex<Vec<Minted>>,
}

struct Minted {
    key_id: i64,
    link: String,
    expires_at: DateTime<Utc>,
}

impl std::fmt::Debug for Minted {
    /// Everything but the link, which carries its token while it lives: a
    /// log line must not.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Minted")
            .field("key_id", &self.key_id)
            .field("link", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl MintedLinks {
    /// `key_create` / `device_token` minted `link` for device `key_id`,
    /// whose token lapses at `expires_at`.
    pub fn register(&self, key_id: i64, link: &str, expires_at: DateTime<Utc>) {
        let mut open = self.lock();
        // A new token's link replaces the device's old one, which no longer
        // pairs anything.
        open.retain(|m| m.key_id != key_id);
        open.push(Minted {
            key_id,
            link: link.to_string(),
            expires_at,
        });
    }

    /// Take `link` when this process minted exactly it, has not handed it
    /// off yet and its token lives: `true` once, `false` ever after and for
    /// any other link.
    pub fn take(&self, link: &str) -> bool {
        self.take_at(link, Utc::now())
    }

    /// [`Self::take`] at `now`.
    pub fn take_at(&self, link: &str, now: DateTime<Utc>) -> bool {
        let mut open = self.lock();
        open.retain(|m| m.expires_at > now);
        match open.iter().position(|m| m.link == link) {
            Some(at) => {
                open.remove(at);
                true
            }
            None => false,
        }
    }

    /// Device `key_id`'s token ended, or the device was revoked: its link
    /// pairs nothing any more.
    pub fn forget(&self, key_id: i64) {
        self.lock().retain(|m| m.key_id != key_id);
    }

    /// Every token ended at once (the CA's rotation).
    pub fn forget_all(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Minted>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_link_is_taken_once_while_its_token_lives() {
        let now = Utc::now();
        let later = now + chrono::Duration::minutes(10);
        let m = MintedLinks::default();
        m.register(1, "lmgw-pair:?v=2&token=a", later);
        assert!(!m.take_at("lmgw-pair:?v=2&token=b", now), "not minted");
        assert!(m.take_at("lmgw-pair:?v=2&token=a", now));
        assert!(!m.take_at("lmgw-pair:?v=2&token=a", now), "once");

        m.register(1, "lmgw-pair:?v=2&token=old", later);
        m.register(1, "lmgw-pair:?v=2&token=new", later);
        assert!(
            !m.take_at("lmgw-pair:?v=2&token=old", now),
            "a new token replaces it"
        );
        m.register(2, "lmgw-pair:?v=2&token=two", later);
        m.forget(2);
        assert!(
            !m.take_at("lmgw-pair:?v=2&token=two", now),
            "an ended token forgets it"
        );
        m.register(
            3,
            "lmgw-pair:?v=2&token=three",
            now + chrono::Duration::minutes(1),
        );
        assert!(
            !m.take_at(
                "lmgw-pair:?v=2&token=three",
                now + chrono::Duration::minutes(2)
            ),
            "a lapsed token's link is refused"
        );
        assert!(m.take_at("lmgw-pair:?v=2&token=new", now));
        m.register(4, "lmgw-pair:?v=2&token=four", later);
        m.forget_all();
        assert!(!m.take_at("lmgw-pair:?v=2&token=four", now));
    }

    #[test]
    fn its_debug_redacts_the_links() {
        let m = MintedLinks::default();
        m.register(
            1,
            "lmgw-pair:?v=2&token=lmgw-enrol-4b1d",
            Utc::now() + chrono::Duration::minutes(10),
        );
        let said = format!("{m:?}");
        assert!(!said.contains("4b1d"), "{said}");
        assert!(said.contains("key_id: 1"), "{said}");
    }
}
