//! The pairing links this process minted and has not handed off yet
//! (client-apps design §1.4, review W2-1).
//!
//! lmgw's own window hands an `lmgw-pair:` link to the desktop — which
//! starts whichever client is registered for the scheme — because WebKitGTK
//! launches no scheme handler itself. The same window hosts the Chat's HTML
//! preview, whose scripts can navigate their frame, and the navigation
//! handler cannot tell a frame from the page. A device holds `Chat` and its
//! own key, so "a link carrying some device's key" would let any device put
//! a link with an address of its choosing in front of the owner.
//!
//! So the shell hands off only an **exact** link that `key_create` or
//! `key_rotate` minted here, **once**, and forgets it: a link replayed from a
//! preview can then only be the owner's own fresh one, to the address the
//! owner chose. A key revoked meanwhile takes its link with it.

use std::sync::Mutex;

/// The minted, not yet handed-off links, with the key each pairs.
#[derive(Debug, Default)]
pub struct MintedLinks {
    open: Mutex<Vec<(i64, String)>>,
}

impl MintedLinks {
    /// `key_create` / `key_rotate` minted `link` for key `key_id`.
    pub fn register(&self, key_id: i64, link: &str) {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        // A rotate's new link replaces the key's old one, which no longer
        // pairs anything.
        open.retain(|(id, _)| *id != key_id);
        open.push((key_id, link.to_string()));
    }

    /// Take `link` when this process minted exactly it and has not handed
    /// it off yet: `true` once, `false` ever after and for any other link.
    pub fn take(&self, link: &str) -> bool {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        match open.iter().position(|(_, l)| l == link) {
            Some(at) => {
                open.remove(at);
                true
            }
            None => false,
        }
    }

    /// Key `key_id` was revoked: its link pairs nothing any more.
    pub fn forget(&self, key_id: i64) {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        open.retain(|(id, _)| *id != key_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_link_is_taken_once_and_a_replaced_or_revoked_one_never() {
        let m = MintedLinks::default();
        m.register(1, "lmgw-pair:?v=1&key=a");
        assert!(!m.take("lmgw-pair:?v=1&key=b"), "not minted");
        assert!(m.take("lmgw-pair:?v=1&key=a"));
        assert!(!m.take("lmgw-pair:?v=1&key=a"), "once");

        m.register(1, "lmgw-pair:?v=1&key=old");
        m.register(1, "lmgw-pair:?v=1&key=new");
        assert!(!m.take("lmgw-pair:?v=1&key=old"), "a rotate replaces it");
        m.register(2, "lmgw-pair:?v=1&key=two");
        m.forget(2);
        assert!(!m.take("lmgw-pair:?v=1&key=two"), "a revoke forgets it");
        assert!(m.take("lmgw-pair:?v=1&key=new"));
    }
}
