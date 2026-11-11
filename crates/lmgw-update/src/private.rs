//! A download directory only the user can reach.

use std::io;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

/// `<app>` under `$XDG_RUNTIME_DIR` (a tmpfs the system makes 0700 for the user), created 0700.
///
/// Without `XDG_RUNTIME_DIR` (or with a relative one) it is `<app>-<uid>` under
/// [`std::env::temp_dir`], which is shared: there the directory is accepted only when it is a
/// real directory (not a symlink), owned by this user and without group or other access, so a
/// directory another user pre-created there is refused, not used.
///
/// On Windows it is `<app>` under [`std::env::temp_dir`], which there is the user's own
/// (`%LOCALAPPDATA%\Temp` under the profile), created with a private descriptor and, when it
/// is there already, accepted only when it is a real directory, owned by this user, that no other
/// account may read or change ([`lmgw_private`]'s meaning of private).
#[cfg(unix)]
pub fn private_dir(app: &str) -> io::Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    let dir = match runtime {
        Some(base) => base.join(app),
        // SAFETY: geteuid has no preconditions and cannot fail.
        None => std::env::temp_dir().join(format!("{app}-{}", unsafe { libc::geteuid() })),
    };
    ensure_private(&dir)?;
    Ok(dir)
}

#[cfg(windows)]
pub fn private_dir(app: &str) -> io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(app);
    ensure_private(&dir)?;
    Ok(dir)
}

/// Create `dir` private (one level; its parent exists) and check it is ours and closed.
#[cfg(windows)]
pub(crate) fn ensure_private(dir: &Path) -> io::Result<()> {
    match lmgw_private::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let refuse = |why: String| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a private directory of this user ({why}); refusing to download the \
                 update into it",
                dir.display()
            ),
        )
    };
    match lmgw_private::check_path(dir, lmgw_private::Others::Nothing) {
        Ok(found) if found.is_dir => Ok(()),
        Ok(_) => Err(refuse("it is a file".to_string())),
        Err(e) => Err(refuse(e.to_string())),
    }
}

/// Create `dir` 0700 (one level; its parent exists) and check it is ours and closed.
#[cfg(unix)]
pub(crate) fn ensure_private(dir: &Path) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    // symlink_metadata: a symlink at `dir` is not a directory of ours.
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: as above.
    let uid = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a private directory of this user (owner {}, mode {:04o}); \
                 refusing to download the update into it",
                dir.display(),
                meta.uid(),
                meta.mode() & 0o7777
            ),
        ));
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn a_new_directory_is_private_and_accepted() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("app");
        ensure_private(&dir).unwrap();
        ensure_private(&dir).unwrap();
    }

    #[test]
    fn a_directory_others_may_read_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("app");
        lmgw_private::create_dir(&dir).unwrap();
        let out = std::process::Command::new("icacls")
            .arg(&dir)
            .args(["/grant", "*S-1-5-32-545:(R)"])
            .output()
            .unwrap();
        assert!(out.status.success());
        let e = ensure_private(&dir).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(e.to_string().contains("Users"), "{e}");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn a_new_directory_is_0700_and_accepted() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("app");
        ensure_private(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o7777, 0o700);
        ensure_private(&dir).unwrap();
    }

    #[test]
    fn an_open_directory_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("app");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = ensure_private(&dir).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_symlink_in_its_place_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let real = base.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = base.path().join("app");
        symlink(&real, &link).unwrap();
        let e = ensure_private(&link).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }
}
