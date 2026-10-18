//! The device link's trust (device link design §2, WP1): lmgw's device CA,
//! its files, modes and the wrapping of its key; the server certificates and
//! their cache; device certificates; real rustls handshakes over them; the
//! `devices` settings class and the status block's trust half.
//!
//! No test here reaches the desktop's Secret Service: each uses
//! `MemoryWrapStore`, or a gateway's own default, which answers as a
//! headless start does.

mod ca;
mod handshake;
mod issue;
mod leaf;
mod rotation;
mod settings;
mod status;

use std::path::Path;
use std::sync::Arc;

use lmgw_core::devices::tls::{Ca, MemoryWrapStore, Trust};
use rcgen::{KeyPair, PublicKeyData};

/// A trust whose wrapping keys go to `store`.
pub(crate) fn trust_with(store: &Arc<MemoryWrapStore>) -> Trust {
    let t = Trust::default();
    t.set_store(store.clone());
    t
}

/// A CA made in `dir` by a trust that answers as a headless start does.
pub(crate) async fn plain_ca(dir: &Path) -> (Trust, Arc<Ca>) {
    let t = Trust::default();
    let ca = t.open(dir, true).await.unwrap();
    (t, ca)
}

/// A device's key: EC P-256, as Android's Keystore and Kai desktop make it.
pub(crate) fn device_key() -> KeyPair {
    KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap()
}

/// The DER SubjectPublicKeyInfo enrolment sends for `key`.
pub(crate) fn spki(key: &KeyPair) -> Vec<u8> {
    key.subject_public_key_info()
}

/// The file's mode bits.
pub(crate) fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}
