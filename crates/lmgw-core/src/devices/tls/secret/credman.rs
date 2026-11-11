//! The wrapping keys in Windows Credential Manager (Windows design §4.4,
//! WP6): what the Secret Service is on Linux.
//!
//! Each key is one generic credential of the user's, persisted on this
//! machine only (`CRED_PERSIST_LOCAL_MACHINE`: it does not roam with a
//! roaming profile, as a key that opens one machine's CA file should not),
//! its target name `lmgw:device-ca-wrap:<data dir>|<16 hex digits>` and its
//! blob the key's 64 hex digits. `|` is no character of a Windows path, so the
//! prefix up to it names one data directory exactly, and a search for
//! `<prefix>*` (`CredEnumerateW`'s one kind of filter) finds that directory's
//! keys and no other's. Windows protects the blob with the user's logon
//! secrets (DPAPI), as the Secret Service protects its items with the wallet's
//! password.
//!
//! **One directory, one name** ([`dir_name`]). Windows compares paths without
//! case, and one directory has many spellings (`C:\Users\…`, `c:\users\…`,
//! `\\?\C:\Users\…`, a junction on the way): the data dir is resolved once
//! (canonical, the `\\?\` prefix taken off) before it goes into a name, and a
//! found name is compared with it without case, as Windows compares paths.
//!
//! **Lengths.** The target name holds the whole path, and Credential Manager
//! takes at most `CRED_MAX_GENERIC_TARGET_NAME_LENGTH` (32767) UTF-16 units
//! there; a data dir whose name would be longer is refused with that limit
//! named ([`prefix`]). The comment is a fixed text (its limit,
//! `CRED_MAX_STRING_LENGTH`, is 256 units, which a path would pass).
//!
//! **A logon without credentials has no store.** An SSH logon by key (or any
//! network logon that brought no password) has no credential set in its logon
//! session, and every call answers `ERROR_NO_SUCH_LOGON_SESSION`; that is said
//! as [`StoreError::Unavailable`] in its own words, so the key stays plain
//! (with the warn line) as on a Linux start with no Secret Service.

use std::path::Path;

use secrecy::zeroize::Zeroizing;
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_NOT_FOUND, ERROR_NO_SUCH_LOGON_SESSION, FILETIME,
};
use windows_sys::Win32::Security::Credentials::{
    CredDeleteW, CredEnumerateW, CredFree, CredWriteW, CREDENTIALW,
    CRED_MAX_GENERIC_TARGET_NAME_LENGTH, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

use super::{StoreError, WrapKeyStore};
use crate::devices::tls::wrap::WrapKey;
use crate::erased::Erased;

/// The target names' common start.
const TARGET_PREFIX: &str = "lmgw:device-ca-wrap:";

/// Windows Credential Manager, the user's generic credentials (module doc).
#[derive(Debug, Default)]
pub struct CredentialManagerStore;

/// The `<16 hex digits>` after the `|`.
const ID_DIGITS: usize = 16;

/// The comment every credential carries (module doc: no path in it).
const COMMENT: &str = "lmgw: device CA key wrapping";

/// `dir` as its target names spell it (module doc): canonical when it can be
/// resolved, as given otherwise, without the `\\?\` prefix.
fn dir_name(dir: &Path) -> String {
    let resolved = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    strip_verbatim(&resolved.to_string_lossy())
}

/// `\\?\C:\x` as `C:\x` and `\\?\UNC\server\share` as `\\server\share`;
/// anything else (a `\\?\Volume{…}` path among them) as it is.
fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match path.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_string(),
        _ => path.to_string(),
    }
}

/// Whether `text` starts with `prefix`, compared without case as Windows
/// compares paths: one character against one (a character whose upper case
/// is more than one, `ß`, stands for itself).
fn starts_with_no_case(text: &str, prefix: &str) -> bool {
    fn upper(c: char) -> char {
        let mut u = c.to_uppercase();
        match (u.next(), u.next()) {
            (Some(one), None) => one,
            _ => c,
        }
    }
    let mut text = text.chars();
    prefix
        .chars()
        .all(|p| text.next().is_some_and(|t| upper(t) == upper(p)))
}

/// `dir`'s target names' prefix, up to and with the `|`; refused when a name
/// with it would be longer than Credential Manager takes (module doc).
fn prefix(dir: &Path) -> Result<String, StoreError> {
    let prefix = format!("{TARGET_PREFIX}{}|", dir_name(dir));
    let units = prefix.encode_utf16().count() + ID_DIGITS;
    if units > CRED_MAX_GENERIC_TARGET_NAME_LENGTH as usize {
        return Err(StoreError::Failed(format!(
            "the data directory's path is too long for a Windows Credential Manager target \
             name: lmgw's names are `{TARGET_PREFIX}<data dir>|<{ID_DIGITS} hex digits>`, {units} \
             UTF-16 units for this one, and Credential Manager takes at most \
             {CRED_MAX_GENERIC_TARGET_NAME_LENGTH} (CRED_MAX_GENERIC_TARGET_NAME_LENGTH)"
        )));
    }
    Ok(prefix)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// What a failed call's error means (module doc).
fn cred_error(what: &str, code: u32) -> StoreError {
    if code == ERROR_NO_SUCH_LOGON_SESSION {
        StoreError::Unavailable(format!(
            "Windows Credential Manager has no store in this logon session (ERROR_NO_SUCH_LOGON_SESSION \
             while {what}): a logon that brought no password, such as an SSH logon by key or a \
             task that stores none, has none; a start from the desktop session has one"
        ))
    } else {
        StoreError::Failed(format!(
            "Windows Credential Manager failed while {what}: {}",
            std::io::Error::from_raw_os_error(code as i32)
        ))
    }
}

/// One credential found: its target name and the key it holds, if any.
struct Found {
    target: String,
    key: Option<WrapKey>,
}

/// Every credential of `dir`'s (module doc), an empty list for none.
fn enumerate(dir: &Path) -> Result<Vec<Found>, StoreError> {
    let prefix = prefix(dir)?;
    let filter = wide(&format!("{prefix}*"));
    let mut count = 0u32;
    let mut creds: *mut *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `filter` is NUL-terminated; the array is allocated by the call
    // and freed with CredFree below.
    if unsafe { CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut creds) } == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_NOT_FOUND {
            return Ok(Vec::new());
        }
        return Err(cred_error("searching the wrapping keys", code));
    }
    let mut found = Vec::new();
    for i in 0..count as usize {
        // SAFETY: `creds` holds `count` pointers to credentials, alive until
        // the CredFree.
        let cred = unsafe { &**creds.add(i) };
        if cred.Type != CRED_TYPE_GENERIC || cred.TargetName.is_null() {
            continue;
        }
        // SAFETY: TargetName is NUL-terminated UTF-16.
        let target = unsafe { from_wide_ptr(cred.TargetName) };
        // The filter matches without case, as the comparison here does
        // (module doc); this makes sure the name is one of lmgw's for `dir`.
        if !starts_with_no_case(&target, &prefix) {
            continue;
        }
        let blob = if cred.CredentialBlob.is_null() {
            Zeroizing::new(Vec::new())
        } else {
            // SAFETY: the blob has CredentialBlobSize bytes.
            Zeroizing::new(
                unsafe {
                    std::slice::from_raw_parts(
                        cred.CredentialBlob,
                        cred.CredentialBlobSize as usize,
                    )
                }
                .to_vec(),
            )
        };
        found.push(Found {
            target,
            key: WrapKey::from_hex(&blob),
        });
    }
    // SAFETY: allocated by CredEnumerateW, freed once.
    unsafe { CredFree(creds.cast()) };
    Ok(found)
}

/// A NUL-terminated UTF-16 string as text.
///
/// # Safety
/// `p` points at a NUL-terminated UTF-16 string.
unsafe fn from_wide_ptr(p: *const u16) -> String {
    let mut len = 0;
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

fn add(dir: &Path, key: &WrapKey) -> Result<(), StoreError> {
    let mut id = [0u8; ID_DIGITS / 2];
    aws_lc_rs::rand::fill(&mut id)
        .map_err(|_| StoreError::Failed("no random for the credential's name".into()))?;
    let target = wide(&format!("{}{}", prefix(dir)?, hex::encode(id)));
    let comment = wide(COMMENT);
    let user = wide("lmgw");
    let hex = key.to_hex();
    let blob = hex.as_bytes();
    let cred = CREDENTIALW {
        Flags: 0,
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_ptr() as *mut u16,
        Comment: comment.as_ptr() as *mut u16,
        LastWritten: FILETIME::default(),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_ptr() as *mut u8,
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: std::ptr::null_mut(),
        TargetAlias: std::ptr::null_mut(),
        UserName: user.as_ptr() as *mut u16,
    };
    // SAFETY: every pointer in `cred` points at memory alive for the call;
    // CredWriteW only reads it.
    if unsafe { CredWriteW(&cred, 0) } == 0 {
        return Err(cred_error("storing the wrapping key", unsafe {
            GetLastError()
        }));
    }
    Ok(())
}

fn delete(target: &str) -> Result<(), StoreError> {
    let name = wide(target);
    // SAFETY: `name` is NUL-terminated.
    if unsafe { CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
        let code = unsafe { GetLastError() };
        // Gone already: what a prune wanted.
        if code != ERROR_NOT_FOUND {
            return Err(cred_error("deleting an old wrapping key", code));
        }
    }
    Ok(())
}

impl WrapKeyStore for CredentialManagerStore {
    fn get<'a>(&'a self, dir: &'a Path) -> Erased<'a, Result<Vec<WrapKey>, StoreError>> {
        Box::pin(async move {
            let dir = dir.to_path_buf();
            let found = tokio::task::spawn_blocking(move || enumerate(&dir))
                .await
                .map_err(|e| StoreError::Failed(e.to_string()))??;
            let mut keys = Vec::new();
            for f in found {
                match f.key {
                    Some(k) => keys.push(k),
                    None => tracing::warn!(
                        "the credential {} is named as lmgw's device CA wrapping key but does not \
                         hold 64 hex digits; it is not used",
                        f.target
                    ),
                }
            }
            Ok(keys)
        })
    }

    fn add<'a>(&'a self, dir: &'a Path, key: &'a WrapKey) -> Erased<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let (dir, key) = (dir.to_path_buf(), key.duplicate());
            tokio::task::spawn_blocking(move || add(&dir, &key))
                .await
                .map_err(|e| StoreError::Failed(e.to_string()))?
        })
    }

    fn prune<'a>(
        &'a self,
        dir: &'a Path,
        keep: &'a WrapKey,
    ) -> Erased<'a, Result<usize, StoreError>> {
        Box::pin(async move {
            let (dir, keep) = (dir.to_path_buf(), keep.duplicate());
            tokio::task::spawn_blocking(move || {
                let mut gone = 0;
                for f in enumerate(&dir)? {
                    if f.key.as_ref().is_some_and(|k| k.same(&keep)) {
                        continue;
                    }
                    delete(&f.target)?;
                    gone += 1;
                }
                Ok(gone)
            })
            .await
            .map_err(|e| StoreError::Failed(e.to_string()))?
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn a_verbatim_path_is_named_as_its_plain_spelling() {
        assert_eq!(strip_verbatim(r"\\?\C:\Users\a\lmgw"), r"C:\Users\a\lmgw");
        assert_eq!(
            strip_verbatim(r"\\?\UNC\srv\share\lmgw"),
            r"\\srv\share\lmgw"
        );
        assert_eq!(strip_verbatim(r"C:\Users\a"), r"C:\Users\a");
        assert_eq!(
            strip_verbatim(r"\\?\Volume{0b1c}\lmgw"),
            r"\\?\Volume{0b1c}\lmgw"
        );
    }

    #[test]
    fn a_name_is_compared_without_case_and_only_up_to_the_bar() {
        let p = "lmgw:device-ca-wrap:C:\\Users\\Jürgen\\lmgw|";
        assert!(starts_with_no_case(
            "lmgw:device-ca-wrap:c:\\users\\JÜRGEN\\LMGW|0011",
            p
        ));
        assert!(!starts_with_no_case(
            "lmgw:device-ca-wrap:C:\\Users\\Jürgen\\lmgw-2|00",
            p
        ));
        assert!(!starts_with_no_case("lmgw:device-ca-wrap:C:\\Users", p));
    }

    /// Every spelling of one existing directory is one name.
    #[test]
    fn one_directory_has_one_name() {
        let base = tempfile::tempdir().unwrap();
        let canonical = dir_name(base.path());
        assert!(!canonical.starts_with(r"\\?\"), "{canonical}");
        let upper = PathBuf::from(base.path().to_string_lossy().to_uppercase());
        let verbatim = PathBuf::from(format!(r"\\?\{}", canonical));
        assert!(starts_with_no_case(&dir_name(&upper), &canonical));
        assert_eq!(dir_name(&verbatim), canonical);
    }

    /// A path past what a target name holds is refused with the limit named,
    /// before Credential Manager is asked.
    #[test]
    fn a_data_dir_too_long_for_a_target_name_says_the_limit() {
        let dir = PathBuf::from(format!(r"C:\{}", "d".repeat(32_760)));
        match prefix(&dir) {
            Err(StoreError::Failed(why)) => {
                assert!(why.contains("CRED_MAX_GENERIC_TARGET_NAME_LENGTH"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        let fits = PathBuf::from(format!(r"C:\{}", "d".repeat(400)));
        assert!(prefix(&fits).is_ok());
    }
}
