//! lmgw's device trust (device link design §2): a small private CA whose
//! certificate's key the pairing link pins (`fp`), the server certificates
//! it issues for the device listener, and the client certificates it issues
//! for each device's own key.
//!
//! - **the CA** (`ca`): created at the device listener's first start, its
//!   key wrapped at rest under a key in the session's Secret Service
//!   (`wrap`, `secret`), plain only on a headless start, its files checked
//!   for mode and owner (`files`);
//! - **the server certificates** (`leaf`): one per local address, issued in
//!   memory, re-issued from half their validity, carrying every host of
//!   every device's URL list, each with the device listener's two TLS
//!   configurations (`server_config`);
//! - **the device certificates** (`issue`): for an EC P-256 key, with the
//!   row's `key_hash` and the confirmation code derived from the key;
//! - **the state** (`trust`): what the gateway holds of all of it while it
//!   runs, and the status block's trust half.

mod ca;
mod files;
mod issue;
mod leaf;
mod secret;
mod server_config;
mod trust;
mod wrap;

pub use ca::{fingerprint, Ca, CaFacts, KeyMode, NO_EXPIRY};
pub use files::{TrustFiles, CA_CERT, CA_KEY, CERT_NAME, KEY_NAME, TLS_DIR};
pub use issue::{
    check_public_key, issue_device, issue_device_for, key_hash, DeviceCertificate, IssueError,
    PublicKeyRefused,
};
pub use leaf::{Leaf, LeafCache, LeafSlot, UrlHost};
pub use secret::{
    MemoryWrapStore, NoSecretService, SecretServiceStore, StoreError, WrapKeyStore, APPLICATION,
    ATTR_APPLICATION, ATTR_DATA_DIR, ATTR_KIND, KIND_CA_WRAP,
};
pub use server_config::{provider, ClientAuth, ALPN_HTTP1};
pub use trust::{Trust, TrustError};
pub use wrap::{WrapKey, PLAIN_LABEL, WRAPPED_LABEL};
