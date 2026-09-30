//! The machine-wide build lock (container-builds §5 "Serialization"): one
//! `flock` on `$XDG_RUNTIME_DIR/lmgw-build.lock` (`/tmp/lmgw-build-<uid>.lock`
//! without a runtime dir), so a production lmgw and any number of dev
//! instances build one image at a time between them — a build takes every
//! core and 8–12 GB of RAM (§14), two at once would starve both.
//!
//! The holder names itself in a sidecar file beside the lock
//! (`lmgw-build.lock.holder`, JSON), which is how a waiting run's job detail
//! can say *what* it waits for. The sidecar is advisory: the flock is the
//! lock, and an unreadable sidecar only makes the wait anonymous.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Who holds the lock, as the sidecar records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Holder {
    pub slug: String,
    pub run_id: i64,
    pub pid: u32,
}

impl Holder {
    /// What `waiting_for` shows: `official-master (run 12, pid 4711)`.
    pub fn describe(&self) -> String {
        format!("{} (run {}, pid {})", self.slug, self.run_id, self.pid)
    }
}

/// The held lock. Dropping it clears the sidecar, then releases the flock
/// (closing the file) — in that order, so the next holder's sidecar is never
/// the one deleted.
pub(crate) struct BuildLock {
    file: File,
    sidecar: PathBuf,
}

/// `<lock>.holder`.
pub(crate) fn sidecar_path(lock: &Path) -> PathBuf {
    let mut s: OsString = lock.as_os_str().to_owned();
    s.push(".holder");
    PathBuf::from(s)
}

impl BuildLock {
    /// Take the lock at `path` without waiting: `Ok(None)` while anyone else
    /// — another lmgw, or another run in this one — holds it. Each call opens
    /// the file anew, so two runs in one process exclude each other exactly
    /// as two processes do (a flock belongs to the open file, not the
    /// process).
    pub fn try_acquire(path: &Path) -> Result<Option<Self>, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|e| format!("could not open the build lock {}: {e}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                file,
                sidecar: sidecar_path(path),
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(format!(
                "could not take the build lock {}: {e}",
                path.display()
            )),
        }
    }

    /// Say who holds it (best effort — see the module docs).
    pub fn announce(&self, holder: &Holder) {
        let json = serde_json::to_string(holder).unwrap_or_default();
        if let Err(e) = std::fs::write(&self.sidecar, json) {
            tracing::debug!(
                "could not write the build lock sidecar {}: {e}",
                self.sidecar.display()
            );
        }
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sidecar);
        let _ = self.file.unlock();
    }
}

/// The current holder of the lock at `lock`, if its sidecar says.
pub(crate) fn read_holder(lock: &Path) -> Option<Holder> {
    let text = std::fs::read_to_string(sidecar_path(lock)).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_excludes_a_second_open_and_names_its_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("lmgw-build.lock");
        let a = BuildLock::try_acquire(&path).unwrap().expect("free");
        let holder = Holder {
            slug: "official-master".into(),
            run_id: 7,
            pid: 42,
        };
        a.announce(&holder);
        assert!(BuildLock::try_acquire(&path).unwrap().is_none());
        assert_eq!(read_holder(&path), Some(holder.clone()));
        assert_eq!(holder.describe(), "official-master (run 7, pid 42)");
        drop(a);
        assert_eq!(read_holder(&path), None, "the sidecar goes with the lock");
        assert!(BuildLock::try_acquire(&path).unwrap().is_some());
    }
}
