//! A container-mode start and the image's wrapping key (headless server
//! design §9, WP8): the start installs the secret as the trust's store,
//! wraps a plain key it finds at once when the secret holds a key (D102),
//! and says in one line what it found and did.
//!
//! The desktop wraps a plain key only when the device listener next opens
//! it, because wrapping there may raise the wallet's unlock prompt, which
//! belongs to the moment the owner turns the listener on. The image has no
//! prompt: the secret is a file, so a plain key on the data volume is
//! sealed at the start, the device listener on or off.

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;

use super::super::ca::KeyMode;
use super::super::files::{TrustFiles, TLS_DIR};
use super::super::secret::WrapKeyStore;
use super::super::wrap_secret::{WrapKeySecret, WRAP_KEY_SECRET_HOW, WRAP_KEY_SECRET_NAME};
use super::{load_or_create, read_key_file, State, Trust};

impl Trust {
    /// The start's step for the image's secret `found` on the CA of
    /// `data_dir`: install it as the store, wrap a plain key under it when
    /// it holds a key, and log one line keyed on what the key file on disk
    /// is — no CA yet, already wrapped, wrapped now, plain for want of the
    /// secret, wrapped and the secret missing, or the secret unusable.
    pub async fn start_on_secret(&self, data_dir: &Path, found: WrapKeySecret) {
        let files = TrustFiles::in_data_dir(data_dir);
        let secret = found.path().display().to_string();
        let has_key = found.has_key();
        let problem = found.problem().map(str::to_string);
        let store: Arc<dyn WrapKeyStore> = Arc::new(found);
        self.set_store(store.clone());
        let key = files.key.display();
        let make_it =
            format!("`{WRAP_KEY_SECRET_HOW}`, and Secret={WRAP_KEY_SECRET_NAME} in the unit");

        if let Some(why) = problem {
            tracing::error!(
                "{why}. The device listener cannot make, wrap or open the device CA until the \
                 secret holds a key or the Secret={WRAP_KEY_SECRET_NAME} line leaves the unit"
            );
            return;
        }
        let on_disk = files
            .key
            .exists()
            .then(|| read_key_file(&files).map(|k| k.mode));
        match (on_disk, has_key) {
            (None, true) => tracing::info!(
                "lmgw has no device CA yet: the device listener's first start makes one, its key \
                 wrapped under the wrapping key secret {secret}"
            ),
            (None, false) => tracing::info!(
                "no wrapping key secret is mounted at {secret}: the device CA's key, once the \
                 device listener makes one, is kept plain (0600) in {}. To wrap it: {make_it}",
                data_dir.join(TLS_DIR).display()
            ),
            (Some(Err(e)), _) => tracing::warn!(
                "the device CA's key file is not usable ({e}), so this start leaves it as it is \
                 and cannot wrap it under the wrapping key secret {secret}; the device listener \
                 cannot open the device CA until that is fixed"
            ),
            (Some(Ok(KeyMode::Wrapped)), true) => tracing::info!(
                "the device CA's key in {key} is wrapped under the wrapping key secret {secret}; \
                 the device listener opens it when it starts"
            ),
            (Some(Ok(KeyMode::Wrapped)), false) => tracing::warn!(
                "the device CA's key in {key} is wrapped, and no wrapping key secret is mounted \
                 at {secret}: the device listener cannot open the device CA. Restore the Podman \
                 secret it was wrapped under (and Secret={WRAP_KEY_SECRET_NAME} in the unit), or \
                 rotate the device CA, which makes a new key kept plain"
            ),
            (Some(Ok(KeyMode::Plain)), false) => tracing::warn!(
                "the device CA's key in {key} is plain (0600): no wrapping key secret is mounted \
                 at {secret}. To wrap it: {make_it}; the next start with the secret wraps it"
            ),
            (Some(Ok(KeyMode::Plain)), true) => {
                // Wrapped now: the load re-wraps a plain key and says so in
                // its own line ("… is wrapped under the wrapping key secret
                // … now").
                let _one = self.gate.lock().await;
                match load_or_create(&files, data_dir, &*store, false, Utc::now()).await {
                    Ok(_) => {
                        // The key stays closed until the listener opens it;
                        // the status reads the files again.
                        let mut state = self.state.write().unwrap();
                        if !matches!(*state, State::Ready(_)) {
                            *state = State::Unread;
                        }
                    }
                    Err(e) => tracing::warn!(
                        "the device CA's key in {key} is plain, and this start could not wrap \
                         it under the wrapping key secret {secret}: {e}. The device listener's \
                         start tries again"
                    ),
                }
            }
        }
    }
}
