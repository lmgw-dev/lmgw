//! The install's seal key (WP8 verify V-N2): the secret the seal's
//! configuration fingerprint is an HMAC under ([`super::config_fingerprint`]).
//! A carrier goes to every client key, so an unkeyed hash of a row's address,
//! key and headers would let a client check a guessed provider key offline,
//! and see when the owner rotates one. Keyed, the fingerprint says nothing
//! without the install's secret.
//!
//! The key is 32 random bytes, generated once per install and kept in the
//! gateway's database beside its other install-level secrets (the upstream
//! keys, the owner keys' plaintext), under the `settings` table's
//! `seals:config_key` (`store::seal_key`). No API, op, log line or export
//! reads that row. The gateway installs it here at start ([`install`]),
//! before anything seals, and the egresses, which have the row and nothing
//! else, read it from here: one install per process. A second gateway in one
//! process (the tests') keeps the first one's key, which is as consistent
//! for it as its own; a process that installs none (a unit test) uses one
//! drawn for it.

use std::sync::OnceLock;

/// The key's length in bytes.
pub const SEAL_KEY_LEN: usize = 32;

static KEY: OnceLock<[u8; SEAL_KEY_LEN]> = OnceLock::new();

/// Install this gateway's seal key. The first install in a process wins
/// (module doc); `false` when one was there already.
pub fn install(key: [u8; SEAL_KEY_LEN]) -> bool {
    KEY.set(key).is_ok()
}

/// The installed key, or one drawn for this process when none was.
pub(super) fn key() -> &'static [u8; SEAL_KEY_LEN] {
    KEY.get_or_init(rand::random)
}
