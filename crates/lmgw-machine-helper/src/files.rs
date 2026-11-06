//! The two ways the helper writes a file (§3.2, "What S1 writes, and how"),
//! both through a temporary file in the target directory, so the file gets
//! its directory's SELinux label (`system_conf_t` in `/etc/yum.repos.d`,
//! `cert_t` in `/etc/pki/rpm-gpg`) and no half-written file is ever at the
//! path:
//!
//! - [`create_new`]: the temporary file is linked to the path, which fails
//!   when anything is there, so a file lmgw did not write is never
//!   overwritten;
//! - [`replace_own`]: the temporary file is renamed over lmgw's own file,
//!   which the plan judged lmgw's (its marker, or one of lmgw's key texts)
//!   moments before.
//!
//! The temporary file's name, `.lmgw-<name>-<pid>-<nanoseconds>.tmp`, is one
//! dnf does not read (it reads `*.repo`), and it is gone again on success and
//! on failure.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The directory `path` is in, and its file name.
fn split(path: &Path) -> io::Result<(&Path, String)> {
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) if !dir.as_os_str().is_empty() => {
            Ok((dir, name.to_string_lossy().into_owned()))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a file in a directory", path.display()),
        )),
    }
}

/// `content` in a new temporary file beside `path`, with `mode`, written and
/// synced. Created with `O_CREAT|O_EXCL|O_NOFOLLOW`, so nothing that is there
/// is opened.
fn temp_beside(path: &Path, content: &[u8], mode: u32) -> io::Result<PathBuf> {
    let (dir, name) = split(path)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = dir.join(format!(".lmgw-{name}-{}-{nanos}.tmp", std::process::id()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&tmp)?;
    let written = (|| {
        // The mode exactly, whatever the umask.
        f.set_permissions(Permissions::from_mode(mode))?;
        f.write_all(content)?;
        f.sync_all()
    })();
    match written {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// The directory's entries on disk, after a link or a rename in it.
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Create `path` with `content` and `mode`; never over anything that is
/// there.
pub fn create_new(path: &Path, content: &[u8], mode: u32) -> io::Result<()> {
    let tmp = temp_beside(path, content, mode)?;
    let linked = fs::hard_link(&tmp, path);
    let removed = fs::remove_file(&tmp);
    match linked {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} is there already, and lmgw never overwrites a file it did not write",
                    path.display()
                ),
            ))
        }
        Err(e) => return Err(e),
        Ok(()) => {}
    }
    removed.map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "{} is written, but its temporary copy {} could not be removed: {e}",
                path.display(),
                tmp.display()
            ),
        )
    })?;
    sync_dir(split(path)?.0)
}

/// Replace lmgw's own file at `path` with `content` and `mode`.
pub fn replace_own(path: &Path, content: &[u8], mode: u32) -> io::Result<()> {
    let tmp = temp_beside(path, content, mode)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    sync_dir(split(path)?.0)
}
