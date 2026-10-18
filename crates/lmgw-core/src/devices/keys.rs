//! Which device a client certificate's key is (device link design §4.7,
//! D5): the row whose `key_hash` is `spki:<hex SHA-256 of the key>`, and
//! whether the owner confirmed its enrolment (§3.5).
//!
//! A seam ([`DeviceKeys`]): the gateway's own answer ([`StoredKeys`]) reads
//! the snapshot's rows, which enrolment writes the key into and the owner's
//! Confirm stamps (`confirmed_at`, §3.6); a key enrolled beside a kept one
//! (`DeviceLink::waiting`, §5.2) opens its row unconfirmed until the Confirm
//! makes it the row's key. A suite that maps a certificate it
//! issued itself to a row installs [`MemoryKeys`], which keeps a key and a
//! confirmation of its own beside the row.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::config::{ApiKey, ApiKeyKind, Snapshot};

/// What a certificate's key opens.
#[derive(Debug, Clone)]
pub enum KeyMatch {
    /// No device row holds the key: `device_key_unknown`.
    Unknown,
    /// The device row holding it, and whether its enrolment is confirmed.
    Found { key: Box<ApiKey>, confirmed: bool },
}

/// Finds the device row a certificate's key belongs to.
pub trait DeviceKeys: Send + Sync {
    /// The row whose `key_hash` is `key_hash` (`spki:<hex>`) in `snap`.
    fn find(&self, snap: &Snapshot, key_hash: &str) -> KeyMatch;
}

/// The gateway's: a device row of the snapshot whose `key_hash` is the
/// key's, confirmed when its `confirmed_at` is set, or whose waiting key it
/// is, unconfirmed. A new key waits for its own confirmation, whether it
/// replaced a key that never worked or waits beside a kept one.
#[derive(Debug, Default)]
pub struct StoredKeys;

impl DeviceKeys for StoredKeys {
    fn find(&self, snap: &Snapshot, key_hash: &str) -> KeyMatch {
        snap.api_keys
            .iter()
            .filter(|k| k.kind == ApiKeyKind::Device)
            .find_map(|k| {
                let confirmed = if k.key_hash == key_hash {
                    k.link.confirmed_at.is_some()
                } else if k
                    .link
                    .waiting
                    .as_ref()
                    .is_some_and(|w| w.key_hash == key_hash)
                {
                    false
                } else {
                    return None;
                };
                Some(KeyMatch::Found {
                    key: Box::new(k.clone()),
                    confirmed,
                })
            })
            .unwrap_or(KeyMatch::Unknown)
    }
}

/// Keys mapped to device rows by hand: a test's stand-in for enrolment.
#[derive(Debug, Default)]
pub struct MemoryKeys {
    /// `key_hash` → (the row's id, confirmed).
    rows: Mutex<HashMap<String, (i64, bool)>>,
}

impl MemoryKeys {
    /// `key_hash` belongs to row `key_id`, confirmed or not.
    pub fn set(&self, key_hash: &str, key_id: i64, confirmed: bool) {
        self.rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key_hash.to_string(), (key_id, confirmed));
    }

    /// `key_hash` belongs to no row any more.
    pub fn remove(&self, key_hash: &str) {
        self.rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key_hash);
    }
}

impl DeviceKeys for MemoryKeys {
    fn find(&self, snap: &Snapshot, key_hash: &str) -> KeyMatch {
        let Some((id, confirmed)) = self
            .rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key_hash)
            .copied()
        else {
            return KeyMatch::Unknown;
        };
        snap.api_keys
            .iter()
            .find(|k| k.id == id && k.kind == ApiKeyKind::Device)
            .map_or(KeyMatch::Unknown, |k| KeyMatch::Found {
                key: Box::new(k.clone()),
                confirmed,
            })
    }
}

/// The [`DeviceKeys`] the gateway asks: [`StoredKeys`] until something else
/// is installed.
pub struct KeyLookup(RwLock<Arc<dyn DeviceKeys>>);

impl Default for KeyLookup {
    fn default() -> Self {
        Self(RwLock::new(Arc::new(StoredKeys)))
    }
}

impl std::fmt::Debug for KeyLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyLookup")
    }
}

impl KeyLookup {
    /// Ask `keys` from now on.
    pub fn set(&self, keys: Arc<dyn DeviceKeys>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = keys;
    }

    /// The row `key_hash` opens in `snap`.
    pub fn find(&self, snap: &Snapshot, key_hash: &str) -> KeyMatch {
        let keys = self.0.read().unwrap_or_else(|e| e.into_inner()).clone();
        keys.find(snap, key_hash)
    }
}
