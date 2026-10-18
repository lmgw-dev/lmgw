//! The CA's two files (device link design D2, §2.1): `tls/ca.key` and
//! `tls/ca.crt` in the data directory, in a `0700` directory. The key file
//! is `0600`, created with its mode before a byte is written, and replaced
//! atomically; the certificate is `0644`.
//!
//! The key is read from one open file: opened without following a symbolic
//! link, checked on that descriptor (a regular file, lmgw's user's, no bits
//! for group or others), and read from it, so nothing can be swapped in
//! between the check and the read. A `tls/` directory that is a symbolic
//! link, another user's, or writable by group or others is refused too.
//! Every refusal names the file relative to the data directory
//! (`tls/ca.key`), which is what the status shows to its callers.

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use secrecy::zeroize::Zeroizing;

use super::trust::TrustError;

/// The directory under the data directory.
pub const TLS_DIR: &str = "tls";
/// The CA's key: PEM, wrapped or plain (`wrap`).
pub const CA_KEY: &str = "ca.key";
/// The CA's certificate: PEM.
pub const CA_CERT: &str = "ca.crt";
/// The key file as messages and the status name it.
pub const KEY_NAME: &str = "tls/ca.key";
/// The certificate as messages and the status name it.
pub const CERT_NAME: &str = "tls/ca.crt";
/// The directory as messages name it.
const DIR_NAME: &str = "tls/";

/// The key file's mode.
const KEY_MODE: u32 = 0o600;
/// The certificate's mode: public data, what `curl --cacert` reads.
const CERT_MODE: u32 = 0o644;
/// The directory's mode.
const DIR_MODE: u32 = 0o700;

/// Where one data directory keeps its CA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustFiles {
    pub dir: PathBuf,
    pub key: PathBuf,
    pub cert: PathBuf,
}

fn my_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

impl TrustFiles {
    pub fn in_data_dir(data_dir: &Path) -> Self {
        let dir = data_dir.join(TLS_DIR);
        Self {
            key: dir.join(CA_KEY),
            cert: dir.join(CA_CERT),
            dir,
        }
    }

    /// The directory, created `0700` when it is missing, and checked
    /// ([`Self::check_dir`]) when it is not.
    pub fn ensure_dir(&self) -> Result<(), TrustError> {
        if self.dir.symlink_metadata().is_ok() {
            return self.check_dir();
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(&self.dir)
            .map_err(|e| TrustError::Unreadable(format!("cannot create {DIR_NAME}: {e}")))
    }

    /// `tls/` as §2.1 needs it: a directory, not a symbolic link, lmgw's
    /// user's, and writable by nobody else, who could otherwise replace the
    /// files in it.
    pub fn check_dir(&self) -> Result<(), TrustError> {
        let meta = self
            .dir
            .symlink_metadata()
            .map_err(|e| TrustError::Unreadable(format!("cannot read {DIR_NAME}: {e}")))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(TrustError::BadMode(format!(
                "{DIR_NAME} in the data directory is not a directory of its own (a symbolic \
                 link or a file)"
            )));
        }
        let me = my_uid();
        if meta.uid() != me {
            return Err(TrustError::BadMode(format!(
                "{DIR_NAME} is owned by uid {}, not by lmgw's user (uid {me})",
                meta.uid()
            )));
        }
        let mode = meta.mode() & 0o7777;
        if mode & 0o022 != 0 {
            return Err(TrustError::BadMode(format!(
                "{DIR_NAME} has mode {mode:04o}, which lets group or others replace the CA's \
                 files; it must be 0700 (chmod 700 it)"
            )));
        }
        Ok(())
    }

    /// Whether each file exists (a symbolic link counts, and is refused when
    /// read).
    pub fn present(&self) -> (bool, bool) {
        (
            self.key.symlink_metadata().is_ok(),
            self.cert.symlink_metadata().is_ok(),
        )
    }

    /// The key file's bytes, read from the descriptor its mode and owner
    /// were checked on: opened with `O_NOFOLLOW`, a regular file, lmgw's
    /// user's, no bits for group or others (§2.1).
    pub fn read_key(&self) -> Result<Zeroizing<Vec<u8>>, TrustError> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.key)
            .map_err(|e| match e.raw_os_error() {
                Some(libc::ELOOP) => TrustError::BadMode(format!(
                    "{KEY_NAME} is a symbolic link; the CA key must be a regular file in \
                     lmgw's data directory"
                )),
                _ => TrustError::Unreadable(format!("cannot read {KEY_NAME}: {e}")),
            })?;
        let meta = file
            .metadata()
            .map_err(|e| TrustError::Unreadable(format!("cannot read {KEY_NAME}: {e}")))?;
        if !meta.is_file() {
            return Err(TrustError::BadMode(format!(
                "{KEY_NAME} is not a regular file"
            )));
        }
        let me = my_uid();
        if meta.uid() != me {
            return Err(TrustError::BadMode(format!(
                "{KEY_NAME} is owned by uid {}, not by lmgw's user (uid {me}); the CA key must \
                 be lmgw's own",
                meta.uid()
            )));
        }
        let mode = meta.mode() & 0o7777;
        if mode & 0o077 != 0 {
            return Err(TrustError::BadMode(format!(
                "{KEY_NAME} has mode {mode:04o}, which lets group or others at it; the CA key \
                 must be 0600 (chmod 600 it, or rotate the device CA if it may have been copied)"
            )));
        }
        let mut bytes = Zeroizing::new(Vec::new());
        file.read_to_end(&mut bytes)
            .map_err(|e| TrustError::Unreadable(format!("cannot read {KEY_NAME}: {e}")))?;
        Ok(bytes)
    }

    /// The certificate's bytes.
    pub fn read_cert(&self) -> Result<Vec<u8>, TrustError> {
        std::fs::read(&self.cert)
            .map_err(|e| TrustError::Unreadable(format!("cannot read {CERT_NAME}: {e}")))
    }

    /// Replace the key file with `bytes`, `0600` from its creation.
    pub fn write_key(&self, bytes: &[u8]) -> Result<(), TrustError> {
        write_atomic(&self.key, KEY_NAME, bytes, KEY_MODE)
    }

    /// Replace the certificate with `bytes`, `0644`.
    pub fn write_cert(&self, bytes: &[u8]) -> Result<(), TrustError> {
        write_atomic(&self.cert, CERT_NAME, bytes, CERT_MODE)
    }
}

/// `bytes` into `path` (named `name` in messages) by a temporary file
/// beside it, created with `mode` (the umask can only narrow it; it is set
/// exactly before anything is written), synced, then renamed over `path`
/// and the directory synced: a reader sees the old file or the new one,
/// never half of either. On a failure `path` is as it was.
fn write_atomic(path: &Path, name: &str, bytes: &[u8], mode: u32) -> Result<(), TrustError> {
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
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .map_err(|e| fail("create a temporary file for", e))?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| fail("set the mode of", e))?;
        f.write_all(bytes).map_err(|e| fail("write", e))?;
        f.sync_all().map_err(|e| fail("sync", e))?;
        std::fs::rename(&tmp, path).map_err(|e| fail("replace", e))?;
        if let Ok(d) = std::fs::File::open(dir) {
            // The rename is durable once the directory is; a failure here
            // leaves the file written all the same.
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
