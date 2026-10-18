//! What the gateway holds of its device trust while it runs (device link
//! design §2): the CA as last read or loaded, the leaf cache, and the store
//! of the CA key's wrapping key.
//!
//! The CA is created when the device listener is first turned on, never
//! before ([`Trust::open`] with `create`). A start reads its files only
//! when something asks: the status reads the public facts once ([`Trust::status`],
//! the key file's label and mode, never the Secret Service); the listener's
//! start loads it to sign ([`Trust::open`]), which is when a wrapped key
//! asks the Secret Service and a plain one found with a Secret Service at
//! hand is wrapped (§2.1).

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, Utc};
use lmgw_api_types::device_link as wire;
use rustls::pki_types::CertificateDer;
use secrecy::zeroize::Zeroizing;

use super::ca::{self, Ca, CaFacts, KeyMode};
use super::files::{TrustFiles, CERT_NAME, KEY_NAME};
use super::leaf::LeafCache;
use super::secret::{NoSecretService, StoreError, WrapKeyStore};
use super::wrap::{self, WrapKey, PLAIN_LABEL, WRAPPED_LABEL};

/// Why the CA cannot be used. Each says what it found and where.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    #[error(
        "lmgw has no device CA yet: it is created when the device listener is first turned on"
    )]
    Missing,
    #[error("{0}")]
    Locked(String),
    #[error("{0}")]
    Unreadable(String),
    #[error("{0}")]
    BadMode(String),
    #[error("{0}")]
    NoSecretService(String),
    #[error("{0}")]
    WrapKeyMissing(String),
    #[error("{0}")]
    SecretService(String),
}

impl TrustError {
    pub fn kind(&self) -> wire::DeviceCaErrorKind {
        use wire::DeviceCaErrorKind as K;
        match self {
            Self::Missing => K::Missing,
            Self::Locked(_) => K::Locked,
            Self::Unreadable(_) => K::Unreadable,
            Self::BadMode(_) => K::BadMode,
            Self::NoSecretService(_) => K::NoSecretService,
            Self::WrapKeyMissing(_) => K::WrapKeyMissing,
            Self::SecretService(_) => K::SecretService,
        }
    }
}

/// The CA as the gateway knows it now.
#[derive(Debug, Clone)]
enum State {
    /// Nothing read since the start.
    Unread,
    Missing,
    /// On disk and not loaded: its public facts.
    Stored {
        facts: CaFacts,
        mode: KeyMode,
    },
    Ready(Arc<Ca>),
    /// It cannot be used; what could be read of it, and why.
    Failed {
        facts: Option<CaFacts>,
        mode: Option<KeyMode>,
        error: TrustError,
    },
}

/// lmgw's device trust while it runs.
pub struct Trust {
    store: Mutex<Arc<dyn WrapKeyStore>>,
    state: RwLock<State>,
    /// One load, creation or replacement at a time.
    gate: tokio::sync::Mutex<()>,
    /// The server certificates (§2.2).
    pub leaves: LeafCache,
}

impl Default for Trust {
    /// No Secret Service until the start installs one
    /// ([`Trust::set_store`]): a gateway that is not production's — a dev
    /// instance, a test's — answers as a headless start does.
    fn default() -> Self {
        Self {
            store: Mutex::new(Arc::new(NoSecretService)),
            state: RwLock::new(State::Unread),
            gate: tokio::sync::Mutex::new(()),
            leaves: LeafCache::default(),
        }
    }
}

impl std::fmt::Debug for Trust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Trust")
            .field("state", &*self.state.read().unwrap())
            .field("leaves", &self.leaves)
            .finish()
    }
}

/// The key file's PEM, read and parsed.
struct KeyFile {
    mode: KeyMode,
    contents: Zeroizing<Vec<u8>>,
}

fn read_key_file(files: &TrustFiles) -> Result<KeyFile, TrustError> {
    let text = files.read_key()?;
    let parsed = pem::parse(&*text)
        .map_err(|_| TrustError::Unreadable(format!("{KEY_NAME} is not a PEM file")))?;
    let mode = match parsed.tag() {
        WRAPPED_LABEL => KeyMode::Wrapped,
        PLAIN_LABEL => KeyMode::Plain,
        other => {
            return Err(TrustError::Unreadable(format!(
                "{KEY_NAME} holds a '{other}', not a CA key"
            )))
        }
    };
    Ok(KeyFile {
        mode,
        contents: Zeroizing::new(parsed.into_contents()),
    })
}

fn read_cert_file(files: &TrustFiles) -> Result<CertificateDer<'static>, TrustError> {
    let text = files.read_cert()?;
    let parsed = pem::parse(&text)
        .ok()
        .filter(|p| p.tag() == "CERTIFICATE")
        .ok_or_else(|| TrustError::Unreadable(format!("{CERT_NAME} is not a PEM certificate")))?;
    Ok(CertificateDer::from(parsed.into_contents()))
}

/// What a store's error means for the CA (§2.1).
fn store_error(e: StoreError, wrapped_on_disk: bool) -> TrustError {
    match e {
        StoreError::Unavailable(why) => TrustError::NoSecretService(format!(
            "{KEY_NAME} is wrapped, and no Secret Service answers this start ({why}); start \
             lmgw in the desktop session"
        )),
        StoreError::Declined => TrustError::Locked(format!(
            "the wallet's unlock prompt was declined, so the device CA's key {}; the device \
             listener stays off until it is turned on again with the wallet unlocked",
            if wrapped_on_disk {
                "cannot be opened"
            } else {
                "cannot be wrapped"
            }
        )),
        StoreError::Failed(why) => TrustError::SecretService(why),
    }
}

/// The key `KeyFile` holds, opened: a wrapped one with the store's key.
async fn open_key(
    data_dir: &Path,
    store: &dyn WrapKeyStore,
    key: &KeyFile,
) -> Result<Zeroizing<Vec<u8>>, TrustError> {
    match key.mode {
        KeyMode::Plain => Ok(key.contents.clone()),
        KeyMode::Wrapped => {
            let keys = store
                .get(data_dir)
                .await
                .map_err(|e| store_error(e, true))?;
            if keys.is_empty() {
                return Err(TrustError::WrapKeyMissing(format!(
                    "{KEY_NAME} is wrapped, and the Secret Service holds no wrapping key for \
                     this data directory (its item was deleted, or the wallet replaced); rotate \
                     the device CA to make a new one"
                )));
            }
            keys.iter()
                .find_map(|k| wrap::open(k, &key.contents).ok())
                .ok_or_else(|| {
                    TrustError::Unreadable(format!(
                        "{KEY_NAME}: no wrapping key the Secret Service holds for this data \
                         directory opens it; rotate the device CA"
                    ))
                })
        }
    }
}

/// A key file's text: how it keeps the key, its PEM, and the wrapping key
/// added to the store for it (to keep when the store is pruned after the
/// write).
struct KeyText {
    mode: KeyMode,
    pem: Zeroizing<String>,
    wrap_key: Option<WrapKey>,
}

/// The key file's PEM for `pkcs8`: wrapped under a new wrapping key added
/// to the store first (beside the old items, which still open the file on
/// disk until it is replaced), or plain when no Secret Service answers
/// (§2.1).
async fn key_file_for(
    files: &TrustFiles,
    data_dir: &Path,
    store: &dyn WrapKeyStore,
    pkcs8: &[u8],
) -> Result<KeyText, TrustError> {
    let wrap_key = WrapKey::generate().map_err(TrustError::Unreadable)?;
    match store.add(data_dir, &wrap_key).await {
        Ok(()) => {
            let sealed = wrap::seal(&wrap_key, pkcs8).map_err(TrustError::Unreadable)?;
            Ok(KeyText {
                mode: KeyMode::Wrapped,
                pem: Zeroizing::new(pem::encode(&pem::Pem::new(WRAPPED_LABEL, sealed))),
                wrap_key: Some(wrap_key),
            })
        }
        Err(StoreError::Unavailable(why)) => {
            tracing::warn!(
                "no Secret Service answers ({why}): the device CA's key is kept plain in {} \
                 (0600) until a start that reaches one wraps it",
                files.key.display()
            );
            Ok(KeyText {
                mode: KeyMode::Plain,
                pem: Zeroizing::new(pem::encode(&pem::Pem::new(PLAIN_LABEL, pkcs8.to_vec()))),
                wrap_key: None,
            })
        }
        Err(e) => Err(store_error(e, false)),
    }
}

/// Once the key file wrapped under `keep` is written, the store's other
/// items for this data directory open nothing: they go. A failure here
/// leaves the CA as it is, and only items that open nothing behind; it is
/// said, and the next write prunes again.
async fn prune(data_dir: &Path, store: &dyn WrapKeyStore, keep: &WrapKey) {
    match store.prune(data_dir, keep).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            "deleted {n} old wrapping key item(s) of the device CA from the Secret Service"
        ),
        Err(e) => tracing::warn!(
            "old wrapping key items of the device CA stay in the Secret Service ({e}); they \
             open nothing, and the next write of the key deletes them"
        ),
    }
}

/// Load the CA in `files`, or make one when there is none and `create`.
async fn load_or_create(
    files: &TrustFiles,
    data_dir: &Path,
    store: &dyn WrapKeyStore,
    create: bool,
    now: DateTime<Utc>,
) -> Result<Ca, TrustError> {
    match files.present() {
        (false, false) if create => {
            return make(files, data_dir, store, now).await.map_err(|f| f.error)
        }
        (false, false) => return Err(TrustError::Missing),
        (true, false) => {
            return Err(TrustError::Unreadable(format!(
                "{KEY_NAME} exists and {CERT_NAME} does not (a CA made or rotated half-way); \
                 rotate the device CA"
            )))
        }
        (false, true) => {
            return Err(TrustError::Unreadable(format!(
                "{CERT_NAME} exists and {KEY_NAME} does not; rotate the device CA"
            )))
        }
        (true, true) => {}
    }
    files.check_dir()?;
    let der = read_cert_file(files)?;
    let key_file = read_key_file(files)?;
    let pkcs8 = open_key(data_dir, store, &key_file).await?;
    let mut ca = ca::load(der, &pkcs8, key_file.mode)?;
    if key_file.mode == KeyMode::Plain {
        // A plain key, and maybe a Secret Service at hand now: wrap it and
        // replace the plain file (§2.1).
        let text = key_file_for(files, data_dir, store, &pkcs8).await?;
        if let Some(keep) = &text.wrap_key {
            files.write_key(text.pem.as_bytes())?;
            prune(data_dir, store, keep).await;
            tracing::info!(
                "the device CA's key in {} is wrapped in the Secret Service now; the plain file \
                 was replaced (a copy a snapshot or a backup took of it was not)",
                files.key.display()
            );
            ca = ca::load(ca.certificate().clone(), &pkcs8, KeyMode::Wrapped)?;
        }
    }
    Ok(ca)
}

/// Why [`make`] failed, and whether it had replaced the key file by then:
/// before that, the CA on disk is as it was.
struct MakeFailed {
    error: TrustError,
    wrote: bool,
}

/// A new CA in `files`, replacing whatever is there: the key file first,
/// then the certificate, each replaced atomically. Its wrapping key, when
/// there is one, replaces the store's item for this data directory first,
/// so whatever the files held before no longer opens.
async fn make(
    files: &TrustFiles,
    data_dir: &Path,
    store: &dyn WrapKeyStore,
    now: DateTime<Utc>,
) -> Result<Ca, MakeFailed> {
    let early = |error| MakeFailed {
        error,
        wrote: false,
    };
    let late = |error| MakeFailed { error, wrote: true };
    files.ensure_dir().map_err(early)?;
    let new = ca::generate(now)
        .map_err(TrustError::Unreadable)
        .map_err(early)?;
    let text = key_file_for(files, data_dir, store, &new.pkcs8)
        .await
        .map_err(early)?;
    // Atomic: on a failure the old key file is as it was, and the new item
    // opens nothing (the next write prunes it).
    files.write_key(text.pem.as_bytes()).map_err(early)?;
    let cert_pem = pem::encode(&pem::Pem::new("CERTIFICATE", new.der.to_vec()));
    files.write_cert(cert_pem.as_bytes()).map_err(late)?;
    let ca = ca::load(new.der, &new.pkcs8, text.mode).map_err(late)?;
    if let Some(keep) = &text.wrap_key {
        prune(data_dir, store, keep).await;
    }
    let mode = text.mode;
    tracing::info!(
        "made lmgw's device CA {} in {} (key {})",
        ca.facts().fp_short(),
        files.dir.display(),
        mode.as_str()
    );
    Ok(ca)
}

/// The facts the files show without opening the key: the certificate's
/// and the key file's label and mode (§9.2).
fn peek(files: &TrustFiles) -> State {
    match files.present() {
        (false, false) => return State::Missing,
        (true, true) => {}
        _ => {
            let error = TrustError::Unreadable(format!(
                "only one of {KEY_NAME} and {CERT_NAME} exists; rotate the device CA"
            ));
            return State::Failed {
                facts: None,
                mode: None,
                error,
            };
        }
    }
    if let Err(error) = files.check_dir() {
        return State::Failed {
            facts: None,
            mode: None,
            error,
        };
    }
    let facts = read_cert_file(files).and_then(|der| {
        CaFacts::read(&der).map_err(|why| TrustError::Unreadable(format!("{CERT_NAME}: {why}")))
    });
    let key = read_key_file(files);
    match (facts, key) {
        (Ok(facts), Ok(key)) => State::Stored {
            facts,
            mode: key.mode,
        },
        (facts, key) => {
            let mode = key.as_ref().ok().map(|k| k.mode);
            let error = match (&facts, key) {
                (Err(e), _) => e.clone(),
                (_, Err(e)) => e,
                _ => unreachable!("both read"),
            };
            State::Failed {
                facts: facts.ok(),
                mode,
                error,
            }
        }
    }
}

impl Trust {
    /// Where the CA key's wrapping key is kept from now on: the session's
    /// Secret Service in the app (`AppState::init_with`), a fake in tests.
    pub fn set_store(&self, store: Arc<dyn WrapKeyStore>) {
        *self.store.lock().unwrap() = store;
    }

    fn store(&self) -> Arc<dyn WrapKeyStore> {
        self.store.lock().unwrap().clone()
    }

    /// The CA as loaded, if it is.
    pub fn ca(&self) -> Option<Arc<Ca>> {
        match &*self.state.read().unwrap() {
            State::Ready(ca) => Some(ca.clone()),
            _ => None,
        }
    }

    /// The CA of `data_dir`, loaded to sign: as loaded already, else read
    /// from its files (asking the Secret Service for a wrapped key, and
    /// wrapping a plain one when it answers), else — with `create`, which
    /// the device listener's first start passes — made. A failure is kept
    /// for the status and answered; the next call tries again.
    pub async fn open(&self, data_dir: &Path, create: bool) -> Result<Arc<Ca>, TrustError> {
        let _one = self.gate.lock().await;
        if let Some(ca) = self.ca() {
            return Ok(ca);
        }
        let files = TrustFiles::in_data_dir(data_dir);
        let store = self.store();
        let result = load_or_create(&files, data_dir, &*store, create, Utc::now()).await;
        self.settle(&files, result)
    }

    /// Replace the CA of `data_dir` with a new one (the owner's rotation,
    /// §5.5): a new key under a new wrapping key and a new certificate, the
    /// old key and its wrapping key not kept, and the leaf cache emptied.
    /// Every device paired under the old CA stops verifying lmgw; resetting
    /// their rows is the caller's.
    ///
    /// A failure before the key file is replaced (a declined prompt, the
    /// store failing, a file that cannot be written) leaves everything as it
    /// was (the CA, its state and the leaves) and is answered; one after it
    /// is kept as the state, as an open's would be.
    pub async fn replace(&self, data_dir: &Path) -> Result<Arc<Ca>, TrustError> {
        let _one = self.gate.lock().await;
        let files = TrustFiles::in_data_dir(data_dir);
        let store = self.store();
        match make(&files, data_dir, &*store, Utc::now()).await {
            Err(MakeFailed {
                error,
                wrote: false,
            }) => Err(error),
            result => {
                let settled = self.settle(&files, result.map_err(|f| f.error));
                self.leaves.clear();
                settled
            }
        }
    }

    /// Keep `result` as the state, and answer it.
    fn settle(
        &self,
        files: &TrustFiles,
        result: Result<Ca, TrustError>,
    ) -> Result<Arc<Ca>, TrustError> {
        let mut state = self.state.write().unwrap();
        match result {
            Ok(ca) => {
                let ca = Arc::new(ca);
                *state = State::Ready(ca.clone());
                Ok(ca)
            }
            Err(TrustError::Missing) => {
                *state = State::Missing;
                Err(TrustError::Missing)
            }
            Err(error) => {
                // What the files still show, beside why they cannot serve.
                let (facts, mode) = match peek(files) {
                    State::Stored { facts, mode } => (Some(facts), Some(mode)),
                    State::Failed { facts, mode, .. } => (facts, mode),
                    _ => (None, None),
                };
                *state = State::Failed {
                    facts,
                    mode,
                    error: error.clone(),
                };
                Err(error)
            }
        }
    }

    /// The status block's trust half (§9.2) for the CA of `data_dir`. When
    /// nothing has read the files since the start, their public facts are
    /// read once here; the key is never opened and no Secret Service asked.
    pub fn status(&self, data_dir: &Path) -> wire::DevicesStatus {
        let files = TrustFiles::in_data_dir(data_dir);
        if matches!(*self.state.read().unwrap(), State::Unread) {
            let peeked = peek(&files);
            let mut state = self.state.write().unwrap();
            // Only if nothing loaded it meanwhile.
            if matches!(*state, State::Unread) {
                *state = peeked;
            }
        }
        let state = self.state.read().unwrap().clone();
        let base = wire::DeviceCaStatus {
            key_file: KEY_NAME.to_string(),
            cert_file: CERT_NAME.to_string(),
            ..Default::default()
        };
        let with =
            |state, facts: Option<&CaFacts>, mode: Option<KeyMode>, error: Option<&TrustError>| {
                wire::DeviceCaStatus {
                    state,
                    fp: facts.map(|f| f.fp.clone()),
                    created: facts
                        .map(|f| f.created.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                    key_mode: mode.map(Into::into),
                    error: error.map(|e| wire::DeviceCaError {
                        kind: e.kind(),
                        message: e.to_string(),
                    }),
                    ..base.clone()
                }
            };
        let ca = match &state {
            State::Unread | State::Missing => with(wire::DeviceCaState::Missing, None, None, None),
            State::Stored { facts, mode } => {
                with(wire::DeviceCaState::Stored, Some(facts), Some(*mode), None)
            }
            State::Ready(ca) => with(
                wire::DeviceCaState::Ready,
                Some(ca.facts()),
                Some(ca.key_mode()),
                None,
            ),
            State::Failed { facts, mode, error } => with(
                wire::DeviceCaState::Error,
                facts.as_ref(),
                *mode,
                Some(error),
            ),
        };
        // The certificate is public (`0644`); it is shown whenever the files
        // gave its facts, the key not needed.
        let ca = wire::DeviceCaStatus {
            certificate_pem: ca
                .fp
                .as_ref()
                .and_then(|_| files.read_cert().ok())
                .and_then(|bytes| String::from_utf8(bytes).ok()),
            ..ca
        };
        wire::DevicesStatus {
            ca,
            leaves: self.leaves.leaves().iter().map(|l| l.view()).collect(),
            // The listener's half is the listener's (`DeviceListener::status`),
            // pairing's the tokens' (`devices::status`).
            listener: Default::default(),
            pairing: Default::default(),
        }
    }
}
