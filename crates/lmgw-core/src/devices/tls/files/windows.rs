//! [`TrustFiles`]' checked reads and writes on Windows (Windows design §4.4,
//! WP6), with `lmgw_private`'s meaning of private: owned by the account lmgw
//! runs as, and no account besides it, `SYSTEM` and the Administrators let
//! in. `tls/` is made with a protected DACL of just those (Linux's `0700`)
//! and may let others read but not change it (Linux's `mode & 0o022`); the
//! key is created with that DACL before a byte is written (Linux's `0600`),
//! and read from the handle its owner and DACL were checked on, opened
//! without following a link. The certificate is created inside `tls/`
//! without a DACL of its own, so it has the directory's.

use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsHandle;
use std::path::Path;

use lmgw_private::{NotPrivate, OpenError, Others};
use secrecy::zeroize::Zeroizing;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

use super::{TrustError, TrustFiles, CERT_NAME, DIR_NAME, KEY_NAME};

impl TrustFiles {
    /// The directory, created private when it is missing, and checked
    /// ([`Self::check_dir`]) either way: what stands there after the create
    /// may be another's, made in the moment between the look and the create.
    pub fn ensure_dir(&self) -> Result<(), TrustError> {
        if self.dir.symlink_metadata().is_err() {
            lmgw_private::create_dir_all(&self.dir)
                .map_err(|e| TrustError::Unreadable(format!("cannot create {DIR_NAME}: {e}")))?;
        }
        self.check_dir()
    }

    /// `tls/` as §2.1 needs it: a directory, not a link, lmgw's account's,
    /// and changeable by no other account, who could otherwise replace the
    /// files in it.
    pub fn check_dir(&self) -> Result<(), TrustError> {
        match lmgw_private::check_path(&self.dir, Others::ReadOnly) {
            Ok(found) if found.is_dir => Ok(()),
            Ok(_) | Err(NotPrivate::Link) => Err(TrustError::BadMode(format!(
                "{DIR_NAME} in the data directory is not a directory of its own (a symbolic \
                 link, a junction or a file)"
            ))),
            Err(NotPrivate::Unreadable(e)) => Err(TrustError::Unreadable(format!(
                "cannot read {DIR_NAME}: {e}"
            ))),
            Err(e) => Err(TrustError::BadMode(format!(
                "{DIR_NAME} is not private: {e}, so another account could replace the CA's \
                 files; only the account lmgw runs as, SYSTEM and the Administrators may change \
                 it (icacls {} /inheritance:r /grant:r \"%USERNAME%:(OI)(CI)F\" \
                 \"SYSTEM:(OI)(CI)F\" \"*S-1-5-32-544:(OI)(CI)F\")",
                self.dir.display()
            ))),
        }
    }

    /// The key file's bytes, read from the handle its owner and DACL were
    /// checked on: opened without following a link, a regular file, lmgw's
    /// account's, no other account let in (§2.1).
    pub fn read_key(&self) -> Result<Zeroizing<Vec<u8>>, TrustError> {
        let mut file = lmgw_private::open_no_follow(&self.key, false).map_err(|e| match e {
            OpenError::Link => TrustError::BadMode(format!(
                "{KEY_NAME} is a symbolic link or junction; the CA key must be a regular file in \
                 lmgw's data directory"
            )),
            OpenError::Io(e) => TrustError::Unreadable(format!("cannot read {KEY_NAME}: {e}")),
        })?;
        let meta = file
            .metadata()
            .map_err(|e| TrustError::Unreadable(format!("cannot read {KEY_NAME}: {e}")))?;
        if !meta.is_file() {
            return Err(TrustError::BadMode(format!(
                "{KEY_NAME} is not a regular file"
            )));
        }
        lmgw_private::check_handle(file.as_handle(), Others::Nothing).map_err(|e| match e {
            NotPrivate::Unreadable(e) => {
                TrustError::Unreadable(format!("cannot read {KEY_NAME}: {e}"))
            }
            e => TrustError::BadMode(format!(
                "{KEY_NAME} is not private: {e}; the CA key must be readable by the account lmgw \
                 runs as alone (rotate the device CA if it may have been copied)"
            )),
        })?;
        let mut bytes = Zeroizing::new(Vec::new());
        file.read_to_end(&mut bytes)
            .map_err(|e| TrustError::Unreadable(format!("cannot read {KEY_NAME}: {e}")))?;
        Ok(bytes)
    }

    /// Replace the key file with `bytes`, private from its creation.
    pub fn write_key(&self, bytes: &[u8]) -> Result<(), TrustError> {
        write_atomic(&self.key, KEY_NAME, bytes, true)
    }

    /// Replace the certificate with `bytes`, with `tls/`'s access.
    pub fn write_cert(&self, bytes: &[u8]) -> Result<(), TrustError> {
        write_atomic(&self.cert, CERT_NAME, bytes, false)
    }
}

/// `bytes` into `path` (named `name` in messages) by a new temporary file
/// beside it (`CREATE_NEW`, no link followed; `private`: the private DACL
/// from its creation), synced, then renamed over `path`: a reader sees the
/// old file or the new one. NTFS journals the rename itself; Windows has no
/// directory handle to sync as Linux does. On a failure `path` is as it was.
fn write_atomic(path: &Path, name: &str, bytes: &[u8], private: bool) -> Result<(), TrustError> {
    let fail = |what: &str, e: std::io::Error| {
        TrustError::Unreadable(format!("cannot {what} {name}: {e}"))
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut nonce = [0u8; 8];
    aws_lc_rs::rand::fill(&mut nonce).map_err(|_| {
        fail(
            "name a temporary file for",
            std::io::Error::other("no random"),
        )
    })?;
    let tmp = dir.join(format!(".{file_name}.{}.tmp", hex::encode(nonce)));
    let result = (|| {
        let mut f = if private {
            lmgw_private::create_new_file(&tmp)
        } else {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&tmp)
        }
        .map_err(|e| fail("create a temporary file for", e))?;
        f.write_all(bytes).map_err(|e| fail("write", e))?;
        f.sync_all().map_err(|e| fail("sync", e))?;
        drop(f);
        std::fs::rename(&tmp, path).map_err(|e| fail("replace", e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
