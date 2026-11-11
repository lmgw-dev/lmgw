//! The server image's wrapping key (headless server design §9, WP8): one
//! fixed key from a file the container is handed, the Podman secret
//! `lmgw-wrap-key` that the unit's `Secret=lmgw-wrap-key` mounts at
//! [`WRAP_KEY_SECRET`]. A copy of the data volume alone then does not carry
//! the device CA's key.
//!
//! The file is read-only and holds one key, so the store has nothing to add
//! or prune: the CA's key file is sealed under that key whenever it is
//! written ([`WrapKeyStore::fixed_key`]), and adding or pruning is a no-op.
//! What the start found decides how the store answers:
//!
//! - **the key**: every wrap and open uses it;
//! - **no file**: as a start without a Secret Service answers — a new or
//!   plain key is kept plain with the warn line, and a wrapped one cannot
//!   be opened, said with the secret's name (it was removed, or the unit
//!   lost its `Secret=` line);
//! - **a file that is no key** (unreadable, not hex, not 64 hex digits):
//!   every call fails with what is wrong, so nothing is made, wrapped or
//!   kept plain until it is fixed.
//!
//! One key for every write means an older copy of the key file (a backup
//! of the data volume, a snapshot) keeps opening under it: a rotation of
//! the CA does not end that, as pruning the old item does under the
//! Secret Service. A new secret does (headless server design D98): with
//! the secret replaced, the start finds the key file unreadable, and the
//! owner's rotation seals a new key under the new secret.

use std::path::{Path, PathBuf};

use secrecy::zeroize::Zeroizing;

use super::secret::{StoreError, WrapKeyStore};
use super::wrap::WrapKey;
use crate::erased::Erased;

/// The Podman secret's name, as the unit's `Secret=` names it.
pub const WRAP_KEY_SECRET_NAME: &str = "lmgw-wrap-key";
/// Where Podman mounts it: `/run/secrets/<name>`, its default target. It is
/// there under `--systemd=always`'s `/run` tmpfs, which Podman mounts first
/// (seen 2026-11-08, Podman 6.1.3).
pub const WRAP_KEY_SECRET: &str = "/run/secrets/lmgw-wrap-key";
/// How the owner makes the secret, as the messages say it.
pub const WRAP_KEY_SECRET_HOW: &str = "openssl rand -hex 32 | podman secret create lmgw-wrap-key -";
/// The key's length in hex digits: 256 bits, AES-256-GCM's key.
const HEX_LEN: usize = 64;

/// The image's wrapping key file, as the start found it.
pub struct WrapKeySecret {
    path: PathBuf,
    found: Found,
}

enum Found {
    Key(WrapKey),
    Absent,
    Broken(String),
}

impl std::fmt::Debug for WrapKeySecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let found = match &self.found {
            Found::Key(_) => "key [redacted]",
            Found::Absent => "absent",
            Found::Broken(_) => "broken",
        };
        f.debug_struct("WrapKeySecret")
            .field("path", &self.path)
            .field("found", &found)
            .finish()
    }
}

/// The key in `bytes`, read from `path`: 64 hex digits, either case, with
/// whitespace around them (the newline `openssl rand -hex` writes) let be.
fn parse(path: &Path, bytes: &[u8]) -> Result<WrapKey, String> {
    let text = bytes.trim_ascii();
    let p = path.display();
    if text.iter().any(|b| !b.is_ascii_hexdigit()) {
        return Err(format!(
            "{p} does not hold a wrapping key: it has characters that are not hex digits; the \
             key is {HEX_LEN} hex digits (256 bits, for AES-256-GCM). Make one with \
             `{WRAP_KEY_SECRET_HOW}`"
        ));
    }
    if text.len() != HEX_LEN {
        return Err(format!(
            "{p} holds {} hex digits; the wrapping key is exactly {HEX_LEN} (256 bits, for \
             AES-256-GCM). Make one with `{WRAP_KEY_SECRET_HOW}`",
            text.len()
        ));
    }
    WrapKey::from_hex(text).ok_or_else(|| format!("{p} does not decode as a wrapping key"))
}

impl WrapKeySecret {
    /// The file at `path` ([`WRAP_KEY_SECRET`] in the image; a test's own
    /// file in tests), read once.
    pub fn read(path: &Path) -> Self {
        let found = match std::fs::read(path) {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                match parse(path, &bytes) {
                    Ok(key) => Found::Key(key),
                    Err(why) => Found::Broken(why),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Found::Absent,
            Err(e) => Found::Broken(format!(
                "the wrapping key secret {} cannot be read: {e}",
                path.display()
            )),
        };
        Self {
            path: path.to_path_buf(),
            found,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether a usable key was found.
    pub fn has_key(&self) -> bool {
        matches!(self.found, Found::Key(_))
    }

    /// Whether there was no file at all.
    pub fn is_absent(&self) -> bool {
        matches!(self.found, Found::Absent)
    }

    /// What is wrong with a file that is there and holds no key.
    pub fn problem(&self) -> Option<&str> {
        match &self.found {
            Found::Broken(why) => Some(why),
            _ => None,
        }
    }

    fn absent(&self) -> StoreError {
        StoreError::Unavailable(format!(
            "no wrapping key secret is mounted at {}",
            self.path.display()
        ))
    }
}

impl WrapKeyStore for WrapKeySecret {
    fn get<'a>(&'a self, _dir: &'a Path) -> Erased<'a, Result<Vec<WrapKey>, StoreError>> {
        Box::pin(async move {
            match &self.found {
                Found::Key(k) => Ok(vec![k.duplicate()]),
                // Holds none: a wrapped key file is the "wrapping key
                // missing" case, said with the secret's name.
                Found::Absent => Ok(Vec::new()),
                Found::Broken(why) => Err(StoreError::Failed(why.clone())),
            }
        })
    }

    fn add<'a>(&'a self, _dir: &'a Path, _key: &'a WrapKey) -> Erased<'a, Result<(), StoreError>> {
        Box::pin(async move {
            match &self.found {
                Found::Key(_) => Ok(()),
                Found::Absent => Err(self.absent()),
                Found::Broken(why) => Err(StoreError::Failed(why.clone())),
            }
        })
    }

    fn prune<'a>(
        &'a self,
        _dir: &'a Path,
        _keep: &'a WrapKey,
    ) -> Erased<'a, Result<usize, StoreError>> {
        Box::pin(async move {
            match &self.found {
                Found::Broken(why) => Err(StoreError::Failed(why.clone())),
                _ => Ok(0),
            }
        })
    }

    fn fixed_key(&self) -> Option<Result<&WrapKey, StoreError>> {
        Some(match &self.found {
            Found::Key(k) => Ok(k),
            Found::Absent => Err(self.absent()),
            Found::Broken(why) => Err(StoreError::Failed(why.clone())),
        })
    }

    fn secret_file(&self) -> Option<&Path> {
        Some(&self.path)
    }
}
