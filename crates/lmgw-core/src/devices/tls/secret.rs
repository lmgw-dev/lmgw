//! Where the CA key's wrapping key lives (device link design D2, §2.1): an
//! item in the session's Secret Service, in the default collection, with the
//! attributes `application: lmgw`, `kind: device-ca-wrap` and the data
//! directory, so a dev instance has an item of its own.
//!
//! A new wrapping key is added beside the items there are, never in their
//! place ([`WrapKeyStore::add`]), and the others are deleted only once the
//! key file it wraps is written ([`WrapKeyStore::prune`]): a crash between
//! the two leaves the old file openable by the old item, and a provider that
//! ignores `replace` piles nothing up.
//!
//! The store is a seam ([`WrapKeyStore`]): a production start installs
//! [`SecretServiceStore`] (`AppState::init_with`, `state::wrap_key_store`);
//! every other gateway — a dev instance (the headless runner, a debug build,
//! `LMGW_DEV`) and the test gateways included — starts with
//! [`NoSecretService`], which answers as a headless start does, so no dev
//! instance, test or tool reaches the desktop's wallet unless it installs a
//! store itself ([`MemoryWrapStore`] in tests).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use secrecy::zeroize::Zeroizing;

use super::wrap::WrapKey;
use crate::erased::Erased;

/// The item's `application` attribute.
pub const ATTR_APPLICATION: &str = "application";
pub const APPLICATION: &str = "lmgw";
/// The item's `kind` attribute.
pub const ATTR_KIND: &str = "kind";
pub const KIND_CA_WRAP: &str = "device-ca-wrap";
/// The item's data-directory attribute: the data directory's absolute path.
pub const ATTR_DATA_DIR: &str = "data_dir";

/// Why a store gave no answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// No Secret Service answers: no session bus, or no provider on it. A
    /// headless start.
    #[error("no Secret Service answers ({0})")]
    Unavailable(String),
    /// The unlock prompt was dismissed, or closed before it was answered.
    #[error("the wallet's unlock prompt was declined")]
    Declined,
    /// Anything else the service answered.
    #[error("the Secret Service failed: {0}")]
    Failed(String),
}

/// The wrapping keys' home.
pub trait WrapKeyStore: Send + Sync + 'static {
    /// The wrapping keys stored for data directory `dir`: every item that
    /// carries lmgw's attributes for it (normally one; the key file opens
    /// with one of them). A locked collection brings the desktop's unlock
    /// prompt.
    fn get<'a>(&'a self, dir: &'a Path) -> Erased<'a, Result<Vec<WrapKey>, StoreError>>;

    /// Add `key` as a new item for `dir`, beside the items there are. A
    /// locked collection brings the desktop's unlock prompt.
    fn add<'a>(&'a self, dir: &'a Path, key: &'a WrapKey) -> Erased<'a, Result<(), StoreError>>;

    /// Delete `dir`'s items that do not hold `keep`, once the key file
    /// `keep` wraps is written; how many went.
    fn prune<'a>(
        &'a self,
        dir: &'a Path,
        keep: &'a WrapKey,
    ) -> Erased<'a, Result<usize, StoreError>>;
}

/// The attributes of `dir`'s item.
fn attributes(dir: &str) -> HashMap<&str, &str> {
    HashMap::from([
        (ATTR_APPLICATION, APPLICATION),
        (ATTR_KIND, KIND_CA_WRAP),
        (ATTR_DATA_DIR, dir),
    ])
}

/// Whether an item carries lmgw's attributes for `dir`. A search matches
/// every item that carries at least the asked ones; the three are checked
/// again here, and any others a provider adds are let be. A foreign item
/// could not pass for the key anyway: AES-GCM authenticates the key file.
fn is_ours(attrs: &HashMap<String, String>, dir: &str) -> bool {
    attrs.get(ATTR_APPLICATION).map(String::as_str) == Some(APPLICATION)
        && attrs.get(ATTR_KIND).map(String::as_str) == Some(KIND_CA_WRAP)
        && attrs.get(ATTR_DATA_DIR).map(String::as_str) == Some(dir)
}

fn dir_text(dir: &Path) -> String {
    dir.to_string_lossy().into_owned()
}

/// The session's Secret Service over D-Bus (KWallet's `ksecretd` on
/// Plasma). A session is opened per call: the store is asked when the
/// device listener starts and when the CA is made, wrapped or rotated, not
/// per request.
#[derive(Debug, Default)]
pub struct SecretServiceStore;

fn ss_error(context: &str, e: secret_service::Error) -> StoreError {
    use secret_service::Error as E;
    match e {
        E::Unavailable => StoreError::Unavailable(e.to_string()),
        E::Prompt | E::PromptDisconnected => StoreError::Declined,
        E::NoResult => StoreError::Failed(format!(
            "{context}: the Secret Service has no default collection (create a wallet in \
             KWallet, or a default keyring)"
        )),
        other => {
            let text = other.to_string();
            // A session bus with no Secret Service provider on it answers
            // the session's opening with D-Bus's own "no such service":
            // nobody answers, as on a headless start.
            if text.contains("ServiceUnknown") || text.contains("NameHasNoOwner") {
                StoreError::Unavailable(text)
            } else {
                StoreError::Failed(format!("{context}: {text}"))
            }
        }
    }
}

async fn connect() -> Result<secret_service::SecretService<'static>, StoreError> {
    secret_service::SecretService::connect(secret_service::EncryptionType::Dh)
        .await
        .map_err(|e| ss_error("opening a Secret Service session", e))
}

/// Every item of `dir`'s, unlocked: the search, and the desktop's unlock
/// prompt for the locked ones (§2.1; a decline is said as one).
async fn our_items<'a>(
    ss: &'a secret_service::SecretService<'a>,
    dir: &str,
) -> Result<Vec<secret_service::Item<'a>>, StoreError> {
    let found = ss
        .search_items(attributes(dir))
        .await
        .map_err(|e| ss_error("searching the wrapping key", e))?;
    if !found.locked.is_empty() {
        let locked: Vec<&secret_service::Item<'_>> = found.locked.iter().collect();
        ss.unlock_all(&locked)
            .await
            .map_err(|e| ss_error("unlocking the wallet", e))?;
    }
    let mut ours = Vec::new();
    for item in found.unlocked.into_iter().chain(found.locked) {
        let attrs = item
            .get_attributes()
            .await
            .map_err(|e| ss_error("reading the wrapping key's item", e))?;
        if is_ours(&attrs, dir) {
            ours.push(item);
        }
    }
    Ok(ours)
}

/// The wrapping key an item holds; `None` for content that is none.
async fn key_of(item: &secret_service::Item<'_>) -> Result<Option<WrapKey>, StoreError> {
    let secret = Zeroizing::new(
        item.get_secret()
            .await
            .map_err(|e| ss_error("reading the wrapping key", e))?,
    );
    Ok(WrapKey::from_hex(&secret))
}

impl WrapKeyStore for SecretServiceStore {
    fn get<'a>(&'a self, dir: &'a Path) -> Erased<'a, Result<Vec<WrapKey>, StoreError>> {
        Box::pin(async move {
            let dir = dir_text(dir);
            let ss = connect().await?;
            let mut keys = Vec::new();
            for item in our_items(&ss, &dir).await? {
                match key_of(&item).await? {
                    Some(k) => keys.push(k),
                    None => tracing::warn!(
                    "an item named as lmgw's device CA wrapping key for {dir} does not hold 64 \
                     hex digits; it is not used"
                ),
                }
            }
            Ok(keys)
        })
    }

    fn add<'a>(&'a self, dir: &'a Path, key: &'a WrapKey) -> Erased<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let dir = dir_text(dir);
            let ss = connect().await?;
            let collection = ss
                .get_default_collection()
                .await
                .map_err(|e| ss_error("reading the default collection", e))?;
            if collection
                .is_locked()
                .await
                .map_err(|e| ss_error("reading whether the wallet is locked", e))?
            {
                collection
                    .unlock()
                    .await
                    .map_err(|e| ss_error("unlocking the wallet", e))?;
            }
            let label = format!("lmgw: device CA key wrapping ({dir})");
            collection
                .create_item(
                    &label,
                    attributes(&dir),
                    key.to_hex().as_bytes(),
                    false,
                    "text/plain",
                )
                .await
                .map_err(|e| ss_error("storing the wrapping key", e))?;
            Ok(())
        })
    }

    fn prune<'a>(
        &'a self,
        dir: &'a Path,
        keep: &'a WrapKey,
    ) -> Erased<'a, Result<usize, StoreError>> {
        Box::pin(async move {
            let dir = dir_text(dir);
            let ss = connect().await?;
            let mut gone = 0;
            for item in our_items(&ss, &dir).await? {
                if key_of(&item).await?.is_some_and(|k| k.same(keep)) {
                    continue;
                }
                item.delete()
                    .await
                    .map_err(|e| ss_error("deleting an old wrapping key", e))?;
                gone += 1;
            }
            Ok(gone)
        })
    }
}

/// No Secret Service: every call answers as a headless start's would. What
/// a gateway holds until its start installs another store.
#[derive(Debug, Default)]
pub struct NoSecretService;

fn not_started() -> StoreError {
    StoreError::Unavailable("this gateway was started without one".into())
}

impl WrapKeyStore for NoSecretService {
    fn get<'a>(&'a self, _dir: &'a Path) -> Erased<'a, Result<Vec<WrapKey>, StoreError>> {
        Box::pin(async move { Err(not_started()) })
    }

    fn add<'a>(&'a self, _dir: &'a Path, _key: &'a WrapKey) -> Erased<'a, Result<(), StoreError>> {
        Box::pin(async move { Err(not_started()) })
    }

    fn prune<'a>(
        &'a self,
        _dir: &'a Path,
        _keep: &'a WrapKey,
    ) -> Erased<'a, Result<usize, StoreError>> {
        Box::pin(async move { Err(not_started()) })
    }
}

/// A store in memory, for tests: the items by data directory, and switches
/// for a store nobody answers for, a declined unlock and a failure. Its
/// `Debug` shows how many items there are, never a key.
#[doc(hidden)]
#[derive(Default)]
pub struct MemoryWrapStore {
    items: Mutex<HashMap<PathBuf, Vec<String>>>,
    unavailable: std::sync::atomic::AtomicBool,
    declined: std::sync::atomic::AtomicBool,
    fail: Mutex<Option<String>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for MemoryWrapStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let items = self.items.lock().unwrap();
        f.debug_struct("MemoryWrapStore")
            .field("dirs", &items.len())
            .field("keys", &"[redacted]")
            .finish()
    }
}

impl MemoryWrapStore {
    /// How many times it was asked, whatever it answered.
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Answer as no Secret Service does (a headless start).
    pub fn set_unavailable(&self, on: bool) {
        self.unavailable
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// Answer as a declined unlock prompt does.
    pub fn set_declined(&self, on: bool) {
        self.declined.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// Answer every call with this failure.
    pub fn set_failure(&self, why: Option<&str>) {
        *self.fail.lock().unwrap() = why.map(String::from);
    }

    /// Delete `dir`'s items, as an owner who cleared the wallet would.
    pub fn forget(&self, dir: &Path) {
        self.items.lock().unwrap().remove(dir);
    }

    /// How many items `dir` has.
    pub fn count(&self, dir: &Path) -> usize {
        self.items.lock().unwrap().get(dir).map_or(0, Vec::len)
    }

    fn check(&self) -> Result<(), StoreError> {
        use std::sync::atomic::Ordering::SeqCst;
        self.calls.fetch_add(1, SeqCst);
        if self.unavailable.load(SeqCst) {
            return Err(StoreError::Unavailable("no session bus (test)".into()));
        }
        if self.declined.load(SeqCst) {
            return Err(StoreError::Declined);
        }
        if let Some(why) = self.fail.lock().unwrap().clone() {
            return Err(StoreError::Failed(why));
        }
        Ok(())
    }
}

impl WrapKeyStore for MemoryWrapStore {
    fn get<'a>(&'a self, dir: &'a Path) -> Erased<'a, Result<Vec<WrapKey>, StoreError>> {
        Box::pin(async move {
            self.check()?;
            Ok(self
                .items
                .lock()
                .unwrap()
                .get(dir)
                .into_iter()
                .flatten()
                .filter_map(|hex| WrapKey::from_hex(hex.as_bytes()))
                .collect())
        })
    }

    fn add<'a>(&'a self, dir: &'a Path, key: &'a WrapKey) -> Erased<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.check()?;
            self.items
                .lock()
                .unwrap()
                .entry(dir.to_path_buf())
                .or_default()
                .push(key.to_hex().to_string());
            Ok(())
        })
    }

    fn prune<'a>(
        &'a self,
        dir: &'a Path,
        keep: &'a WrapKey,
    ) -> Erased<'a, Result<usize, StoreError>> {
        Box::pin(async move {
            self.check()?;
            let mut items = self.items.lock().unwrap();
            let Some(v) = items.get_mut(dir) else {
                return Ok(0);
            };
            let before = v.len();
            v.retain(|hex| WrapKey::from_hex(hex.as_bytes()).is_some_and(|k| k.same(keep)));
            Ok(before - v.len())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// A search matches supersets: an item counts when it carries lmgw's
    /// three attributes for this data directory, whatever else a provider
    /// adds.
    #[test]
    fn an_item_with_lmgws_attributes_for_this_directory_counts() {
        let ours = [
            ("application", "lmgw"),
            ("kind", "device-ca-wrap"),
            ("data_dir", "/d"),
        ];
        assert!(is_ours(&attrs(&ours), "/d"));
        let mut extra = ours.to_vec();
        extra.push(("xdg:schema", "x"));
        extra.push(("folder", "copy"));
        assert!(is_ours(&attrs(&extra), "/d"));
        assert!(!is_ours(&attrs(&ours), "/other"));
        assert!(!is_ours(
            &attrs(&[
                ("application", "lmgw"),
                ("kind", "device-key"),
                ("data_dir", "/d")
            ]),
            "/d"
        ));
        assert!(!is_ours(&attrs(&ours[1..]), "/d"));
    }

    #[tokio::test]
    async fn the_memory_store_adds_beside_and_prunes_to_one() {
        let s = MemoryWrapStore::default();
        let d = Path::new("/d");
        let (a, b) = (WrapKey::generate().unwrap(), WrapKey::generate().unwrap());
        s.add(d, &a).await.unwrap();
        s.add(d, &b).await.unwrap();
        assert_eq!(s.get(d).await.unwrap().len(), 2);
        assert_eq!(s.prune(d, &b).await.unwrap(), 1);
        let left = s.get(d).await.unwrap();
        assert!(left.len() == 1 && left[0].same(&b));
    }
}
