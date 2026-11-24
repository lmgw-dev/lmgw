//! Who can call the conversions, and so whether one caller's shares apply
//! (security round A review, S5).
//!
//! The tool pool's caller share (`pool::share`) and the answers' per-caller
//! rule (`budget::answers`) keep room for a second caller. They apply only
//! when a second caller can exist: **"Require API key" is on and more than
//! one caller may call the conversion routes.** A caller is one key that
//! holds `/v1` (a client key, an agent token, a paired device), and all
//! owner keys of one account together (the dashboard's, self-admin's: one
//! person). With the setting off, or with one such caller, one caller may use
//! the whole pool and every answer share, as before the round; lmgw's own
//! Chat and knowledge-base reads then wait behind that caller's batch.
//!
//! The condition is read from the published configuration at every publish
//! ([`configure`], called beside `pool::configure`), so a key added while
//! lmgw runs switches the shares on with the next request; it is never
//! guessed from traffic.

use std::sync::{Arc, Mutex};

use lmgw_api_types::documents::bounds::CallerFacts;

use crate::config::{ApiKey, ApiKeyKind, Snapshot};
use crate::principal::Principal;

/// The share every owner key of `account` counts against.
fn account_share(account: Option<i64>) -> String {
    format!("account:{}", account.unwrap_or(0))
}

/// The share a key's conversions count against: the key's own, or its
/// account's for an owner key. `None` without a key (every keyless request
/// is the one keyless share, `pool::share::KEYLESS`).
pub fn share_of(p: &Principal) -> Option<Arc<str>> {
    match p {
        Principal::Anonymous => None,
        Principal::Key {
            kind: ApiKeyKind::Owner,
            account,
            ..
        } => Some(Arc::from(account_share(*account))),
        Principal::Key { id, .. } => Some(Arc::from(format!("key:{id}"))),
    }
}

/// The callers `keys` hold at `now`: enabled, unexpired keys that hold
/// `/v1`, the owner keys of one account counted once.
pub fn count(keys: &[ApiKey], now: chrono::DateTime<chrono::Utc>) -> u32 {
    let mut seen: Vec<String> = keys
        .iter()
        .filter(|k| k.enabled && crate::policy::check_expiry(k, now).is_ok())
        .filter_map(|k| match k.kind {
            ApiKeyKind::Owner => Some(account_share(k.account_id)),
            ApiKeyKind::Key | ApiKeyKind::Agent | ApiKeyKind::Device => {
                Some(format!("key:{}", k.id))
            }
            ApiKeyKind::Internal => None,
        })
        .collect();
    seen.sort_unstable();
    seen.dedup();
    u32::try_from(seen.len()).unwrap_or(u32::MAX)
}

/// The facts for `snap` at `now`.
pub fn facts_of(snap: &Snapshot, now: chrono::DateTime<chrono::Utc>) -> CallerFacts {
    let sign_in = snap.settings.auth_enabled;
    let callers = count(&snap.api_keys, now);
    CallerFacts {
        sign_in,
        callers,
        shared: sign_in && callers > 1,
    }
}

fn current() -> &'static Mutex<CallerFacts> {
    static NOW: Mutex<CallerFacts> = Mutex::new(CallerFacts {
        sign_in: true,
        callers: 2,
        shared: true,
    });
    &NOW
}

/// Apply `snap`'s callers to the tool pool and the answers in flight.
pub fn configure(snap: &Snapshot) {
    let f = facts_of(snap, chrono::Utc::now());
    *current().lock().unwrap_or_else(|e| e.into_inner()) = f;
    super::pool::global().set_shared(f.shared);
    super::budget::answers::global().set_shared(f.shared);
}

/// What is in force now (Settings › Documents). Before the first publish
/// (tests without a gateway) the shares apply.
pub fn facts() -> CallerFacts {
    *current().lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: i64, kind: ApiKeyKind, account: Option<i64>) -> ApiKey {
        ApiKey {
            id,
            name: format!("example-{id}"),
            enabled: true,
            kind,
            account_id: account,
            ..Default::default()
        }
    }

    #[test]
    fn owner_keys_of_one_account_are_one_caller_and_internal_rows_none() {
        let now = chrono::Utc::now();
        let mut keys = vec![
            key(1, ApiKeyKind::Owner, Some(1)),
            key(2, ApiKeyKind::Owner, Some(1)),
            key(3, ApiKeyKind::Internal, None),
        ];
        assert_eq!(count(&keys, now), 1);
        keys.push(key(4, ApiKeyKind::Key, None));
        assert_eq!(count(&keys, now), 2);
        keys[3].enabled = false;
        assert_eq!(count(&keys, now), 1);
        keys[3].enabled = true;
        keys[3].policy.expires_at = Some("2001-01-01".into());
        assert_eq!(count(&keys, now), 1, "an expired key calls nothing");
        keys.push(key(5, ApiKeyKind::Device, None));
        keys.push(key(6, ApiKeyKind::Owner, Some(2)));
        assert_eq!(count(&keys, now), 3);
    }
}
