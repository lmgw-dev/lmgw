//! The login nonces (principals design §3.4): single-use, short-lived, each
//! naming the account it signs in (headless server design §5.7).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::state::SharedState;

/// How long a login nonce stays exchangeable (§3.4).
///
/// It is the width of the window between the shell minting the nonce and the
/// webview it just created asking for the page — a second, normally. Sixty is
/// what survives a cold start on a loaded machine; it is not a session length,
/// and the cookie the exchange hands back is what lasts.
pub const LOGIN_NONCE_TTL: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------

/// The login nonces this gateway has minted and not yet seen used.
///
/// In-process and per gateway — a field of `AppState` rather than a `static`,
/// for the same reason the ledger's `Desk` is one: two gateways in one test
/// process must not exchange each other's nonces, and a nonce is a credential
/// for whichever gateway answers it.
///
/// Single-use is [`take`](Self::take) removing the entry, and expiry is
/// checked *after* the removal: a nonce presented late is spent, not left
/// lying around for a second try.
pub struct LoginNonces {
    /// Each live nonce: when it was minted and the account it signs in
    /// (headless server design §5.7; the shell's is account 1's, D11).
    live: Mutex<HashMap<String, (Instant, i64)>>,
    /// [`LOGIN_NONCE_TTL`], unless a test shortened it — see [`Self::set_ttl`].
    ttl_ms: AtomicU64,
}

impl Default for LoginNonces {
    fn default() -> Self {
        Self {
            live: Mutex::new(HashMap::new()),
            ttl_ms: AtomicU64::new(LOGIN_NONCE_TTL.as_millis() as u64),
        }
    }
}

impl LoginNonces {
    /// How long a nonce minted now stays exchangeable.
    pub fn ttl(&self) -> Duration {
        Duration::from_millis(self.ttl_ms.load(Ordering::Relaxed))
    }

    /// Shorten (or lengthen) the window — **the seam the expiry test uses**.
    ///
    /// A test that wanted to watch a nonce expire on the real constant would
    /// sleep a minute, so the constant is made injectable instead of the test
    /// being written against a fake clock that the production path never runs.
    /// Nothing in lmgw calls this; it is here so that what the test exercises
    /// is the real `take`, on the real map, with only the width changed.
    pub fn set_ttl(&self, ttl: Duration) {
        self.ttl_ms.store(ttl.as_millis() as u64, Ordering::Relaxed);
    }

    /// A fresh nonce for `account`, remembered until it is used or expires.
    fn mint(&self, account: i64) -> String {
        let nonce = hex::encode(rand::random::<[u8; 32]>());
        let now = Instant::now();
        let ttl = self.ttl();
        let mut live = self.live.lock().expect("the nonce map is never poisoned");
        // One window per nonce, so the map holds as many entries as the owner
        // opened windows in the last minute. Pruning here keeps a long-running
        // shell from accumulating dead ones for the life of the process.
        live.retain(|_, (issued, _)| now.duration_since(*issued) <= ttl);
        live.insert(nonce.clone(), (now, account));
        nonce
    }

    /// Spend a nonce: the account it signs in if it was live, `None` if it
    /// was unknown, already used, or older than the TTL.
    fn take(&self, nonce: &str) -> Option<i64> {
        let issued = self
            .live
            .lock()
            .expect("the nonce map is never poisoned")
            .remove(nonce);
        issued.and_then(|(at, account)| (at.elapsed() <= self.ttl()).then_some(account))
    }
}

/// Mint a login nonce and say so in the process log (§3.4).
///
/// The shell calls this every time it creates its window: the webview's first
/// history entry is then a dead nonce rather than the durable key, so nothing
/// that reads back a browser history finds a credential there.
pub fn mint_login_nonce(state: &SharedState) -> String {
    mint_login_nonce_for(state, crate::accounts::ACCOUNT_ONE)
}

/// A login nonce that signs in `account` (headless server design §5.7:
/// `account_login_link`, which `lmgw-server login-link` calls).
pub fn mint_login_nonce_for(state: &SharedState, account: i64) -> String {
    let nonce = state.login_nonces.mint(account);
    // At `debug`, with the value: the nonce is single-use and short-lived, and
    // an owner debugging a window that opened on the login card needs to see
    // which nonce the shell handed it and how long it had.
    tracing::debug!(
        "minted login nonce {nonce}, single-use, valid {}s",
        state.login_nonces.ttl().as_secs()
    );
    nonce
}

/// Exchange a nonce, once (§3.4): the account it signs in, `None` when it
/// is unknown, spent or expired.
pub fn exchange_nonce(state: &SharedState, nonce: &str) -> Option<i64> {
    let account = state.login_nonces.take(nonce);
    if account.is_none() {
        tracing::debug!("a login nonce was presented that is unknown, spent or expired");
    }
    account
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonce_is_single_use_and_then_expires() {
        let desk = LoginNonces::default();
        assert_eq!(desk.ttl(), LOGIN_NONCE_TTL);

        let nonce = desk.mint(3);
        assert_eq!(desk.take(&nonce), Some(3), "the first exchange");
        assert_eq!(desk.take(&nonce), None, "the second one is a spent nonce");
        assert_eq!(desk.take("not-a-nonce"), None);

        // Expiry, on a window narrow enough to test: `take` is the real one.
        desk.set_ttl(Duration::from_millis(0));
        let nonce = desk.mint(1);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(desk.take(&nonce), None, "older than the TTL");
    }
}
