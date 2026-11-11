//! The data-dir lock (headless server design D28): a gateway without a
//! window (`lmgw-server`, and the headless dev runner) holds an exclusive
//! `flock` on `<data dir>/lmgw-server.lock` while it runs, so a second
//! gateway on the same data dir — a second container on the same volume, a
//! stray one-off run, a dev runner started twice — refuses to start, naming
//! the holder, instead of migrating and writing a database another process
//! serves. A flock on a file in a shared volume holds across containers on
//! one host.
//!
//! The file names its holder: the program, its pid and the host it runs on
//! (in a container, the container's id where Podman says it, else its host
//! name, which Podman sets to the short id), because a pid alone is
//! another pid namespace's number when the holder is another container.
//!
//! The lock file stays when the process ends; the kernel drops the lock with
//! the process, however it ends.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The lock file's name in the data dir.
pub const LOCK_FILE: &str = "lmgw-server.lock";

/// The held lock: released when dropped, or when the process ends.
#[derive(Debug)]
pub struct DataDirLock {
    _file: File,
    path: PathBuf,
}

impl DataDirLock {
    /// Take the lock on `data_dir` (created when it does not exist yet) for
    /// `program`, and write who holds it into the file. Refused, naming the
    /// holder as its file says it, while another process holds it.
    pub fn acquire(data_dir: &Path, program: &str) -> Result<Self, String> {
        Self::try_acquire(data_dir, program)?.map_err(|busy| busy.message)
    }

    /// [`Self::acquire`], telling a lock another process holds (`Ok(Err)`,
    /// with its sentence) from a failure to lock at all (`Err`): the
    /// command line goes through the running gateway in the first case
    /// (headless server design §5.7).
    pub fn try_acquire(data_dir: &Path, program: &str) -> Result<Result<Self, Busy>, String> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| format!("creating the data dir {}: {e}", data_dir.display()))?;
        let path = data_dir.join(LOCK_FILE);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("opening the data-dir lock {}: {e}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut said = String::new();
                let _ = file.read_to_string(&mut said);
                let holder = holder(&said);
                return Ok(Err(Busy {
                    message: format!(
                        "the data dir {} is in use: {holder} holds {}. One gateway serves a data \
                         dir at a time; stop the other one first",
                        data_dir.display(),
                        path.display()
                    ),
                    same_host: holder_host(&said).map(|h| h == this_host()),
                    holder,
                    path,
                }));
            }
            Err(TryLockError::Error(e)) => {
                return Err(format!("locking {}: {e}", path.display()));
            }
        }
        // Ours now: what a refused start names.
        let host = this_host();
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)))
            .and_then(|_| writeln!(file, "{}\n{host}\n{program}", std::process::id()))
            .map_err(|e| format!("writing the holder into {}: {e}", path.display()))?;
        Ok(Ok(Self { _file: file, path }))
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A lock another process holds: the sentence naming it, and its parts.
#[derive(Debug, Clone)]
pub struct Busy {
    /// The whole refusal a second start says.
    pub message: String,
    /// The holder in words: `lmgw-server (pid 7 on 3f2a9c1e04b7)`.
    pub holder: String,
    /// The lock file.
    pub path: PathBuf,
    /// Whether the holder runs where this process does (the same
    /// container, or the same machine outside one), as the file's host
    /// line says; `None` for a file that names no host.
    pub same_host: Option<bool>,
}

/// The holder a lock file names, in words: `lmgw-server (pid 7 on
/// 3f2a9c1e04b7)`, or "another process" for a file this code did not write.
fn holder(said: &str) -> String {
    let mut lines = said.lines().map(str::trim);
    let Some(Ok(pid)) = lines.next().map(str::parse::<u32>) else {
        return "another process".to_string();
    };
    let host = lines.next().filter(|h| !h.is_empty());
    let program = lines
        .next()
        .filter(|p| !p.is_empty())
        .unwrap_or("a gateway");
    match host {
        Some(host) => format!("{program} (pid {pid} on {host})"),
        None => format!("{program} (pid {pid})"),
    }
}

/// The host line of a lock file this code wrote.
fn holder_host(said: &str) -> Option<&str> {
    let mut lines = said.lines().map(str::trim);
    lines.next()?.parse::<u32>().ok()?;
    lines.next().filter(|h| !h.is_empty())
}

/// Where this process runs, for the lock file: the container's id when
/// Podman's `/run/.containerenv` carries one (it does for a privileged
/// container), else the host name, which in a container is the short id
/// Podman gave it.
fn this_host() -> String {
    std::fs::read_to_string("/run/.containerenv")
        .ok()
        .and_then(|env| container_id(&env))
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "an unknown host".to_string())
}

/// The `id="…"` line of a `/run/.containerenv`, when it has a value.
fn container_id(containerenv: &str) -> Option<String> {
    containerenv.lines().find_map(|l| {
        let id = l.strip_prefix("id=")?.trim().trim_matches('"');
        (!id.is_empty()).then(|| id.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The lock is the headless server's (Windows design §4.5); on Windows a
    // file lock is mandatory, so the held file's holder line cannot be read.
    #[cfg(unix)]
    #[test]
    fn a_second_holder_is_refused_naming_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let held = DataDirLock::acquire(dir.path(), "lmgw-server").unwrap();
        let said = std::fs::read_to_string(held.path()).unwrap();
        assert_eq!(
            said.lines().next(),
            Some(std::process::id().to_string().as_str())
        );
        // flock is per open file: a second open, even in this process, is
        // refused while the first is held.
        let e = DataDirLock::acquire(dir.path(), "headless").unwrap_err();
        assert!(
            e.contains(&format!(
                "lmgw-server (pid {} on {})",
                std::process::id(),
                this_host()
            )) && e.contains(LOCK_FILE),
            "{e}"
        );
        let busy = DataDirLock::try_acquire(dir.path(), "headless")
            .unwrap()
            .unwrap_err();
        assert_eq!(busy.same_host, Some(true), "{busy:?}");
        assert!(busy.holder.starts_with("lmgw-server (pid"), "{busy:?}");
        drop(held);
        DataDirLock::acquire(dir.path(), "headless").expect("released with its holder");
    }

    #[test]
    fn a_missing_data_dir_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("new");
        let lock = DataDirLock::acquire(&data, "lmgw-server").unwrap();
        assert!(lock.path().starts_with(&data));
    }

    #[test]
    fn the_holder_is_read_from_what_the_file_says() {
        assert_eq!(
            holder("7\n3f2a9c1e04b7\nlmgw-server\n"),
            "lmgw-server (pid 7 on 3f2a9c1e04b7)"
        );
        // A file that names only the pid.
        assert_eq!(holder("7\n"), "a gateway (pid 7)");
        assert_eq!(holder(""), "another process");
        assert_eq!(
            holder_host("7\n3f2a9c1e04b7\nlmgw-server\n"),
            Some("3f2a9c1e04b7")
        );
        assert_eq!(holder_host("7\n"), None);
        assert_eq!(
            container_id("engine=\"podman-5.6.1\"\nname=\"lmgw\"\nid=\"3f2a9c\"\n"),
            Some("3f2a9c".into())
        );
        assert_eq!(container_id("engine=\"podman-5.6.1\"\nid=\"\"\n"), None);
    }
}
