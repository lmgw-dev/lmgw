//! A LibreOffice run's private directory, the profile it starts from, and
//! the socket it leaves in `/tmp` (`confine.rs`'s module doc says why).
//!
//! - **Names.** Every directory this gateway makes is `lo-<pid>-…` (and
//!   ffmpeg's `ff-<pid>-…`), so a
//!   gateway sweeping what a crashed one left removes only the directories
//!   of processes that no longer run, never those of another gateway on the
//!   same data dir ([`sweep_once`]).
//! - **The profile template** (`lo-<pid>-profile-<random>`, the random part
//!   drawn once per process, so another local user cannot work out its
//!   socket's name from the data dir and the pid; security round B, L5):
//!   made once per process
//!   and spool directory by a confined LibreOffice run with no document
//!   (`--terminate_after_init`), from the seeded settings
//!   (`profile.rs`), and copied into each run's directory. No document ever
//!   ran in it, and no run's profile is used by another. A run that starts
//!   from a new profile would restart LibreOffice once, and the second
//!   start would find the first start's socket, which a confined run cannot
//!   remove.
//! - **The socket**, `/tmp/OSL_PIPE_<uid>_SingleOfficeIPC_<md5>`: the hash
//!   is LibreOffice's (`dp_misc::generateOfficePipeId`: MD5 over the
//!   profile's file URL in UTF-16, each byte in hex without a leading
//!   zero). The gateway removes it once the run has been reaped
//!   ([`Socket`]), and the sweep removes the sockets of the directories it
//!   removes. A socket already there before a run starts ([`free`]), or
//!   another user's there after it ended ([`foreign`]), is another
//!   process's: LibreOffice would hand its work to it and end without a
//!   result, so the run fails as lmgw's, never as the file's
//!   (`LoError::SocketTaken`). A run's own directory has a random name,
//!   but it is on the reader's command line before LibreOffice binds the
//!   socket, so a squat in that window is still possible (not yet closed).

use std::path::{Path, PathBuf};

use md5::Digest as _;

/// The prefix of every directory a gateway makes here for LibreOffice.
pub const RUN_PREFIX: &str = "lo-";

/// The prefixes the sweep knows: LibreOffice's runs and ffmpeg's
/// (`media/ffmpeg.rs`).
const PREFIXES: &[&str] = &[RUN_PREFIX, "ff-", LAYOUT_PREFIX, POPPLER_PREFIX];

/// The layout model's page renders (`layout_model::stage`), under the spool.
pub const LAYOUT_PREFIX: &str = "lay-";
/// A poppler run's working directory (`pdf::geometry`), under the spool.
pub const POPPLER_PREFIX: &str = "pg-";

/// This process's prefix.
pub fn own_prefix() -> String {
    format!("{RUN_PREFIX}{}-", std::process::id())
}

/// The template's directory under `root` (module doc).
pub fn template_dir(root: &Path) -> PathBuf {
    static DRAWN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let random = DRAWN.get_or_init(|| hex::encode(rand::random::<[u8; 6]>()));
    root.join(format!("{}profile-{random}", own_prefix()))
}

/// Whether nothing is at `socket` yet (module doc); the log names a
/// socket that is.
pub fn free(socket: &Path) -> bool {
    if std::fs::symlink_metadata(socket).is_err() {
        return true;
    }
    tracing::warn!(
        "LibreOffice's single-instance socket {} is there before its run: another process \
         holds it, so the run is not started",
        socket.display()
    );
    false
}

/// Whether `socket` is there and another user's (module doc).
pub fn foreign(socket: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(m) = std::fs::symlink_metadata(socket) {
            if m.uid() != uid() {
                tracing::warn!(
                    "LibreOffice's single-instance socket {} belongs to user {}, not to lmgw's",
                    socket.display(),
                    m.uid()
                );
                return true;
            }
        }
        false
    }
    #[cfg(not(unix))]
    {
        let _ = socket;
        false
    }
}

/// `root` made, readable by this user alone.
pub fn make_private(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(root)?;
        if meta.permissions().mode() & 0o077 != 0 && root != std::env::temp_dir() {
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// The profile URL LibreOffice is given for `profile` (`-env:UserInstallation`).
pub fn profile_url(profile: &Path) -> String {
    super::super::probe::file_url(profile)
}

/// LibreOffice's single-instance socket for the profile at `profile`.
pub fn socket_path(profile: &Path) -> PathBuf {
    let url = profile_url(profile);
    let utf16: Vec<u8> = url.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let digest = md5::Md5::digest(&utf16);
    let hex: String = digest.iter().map(|b| format!("{b:x}")).collect();
    PathBuf::from(crate::extract::convert::confine::LIBREOFFICE_SOCKET_DIR)
        .join(format!("OSL_PIPE_{}_SingleOfficeIPC_{hex}", uid()))
}

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

/// Removes a run's socket when dropped: held by the run's directory, so it
/// goes once the run has been reaped.
pub struct Socket(pub PathBuf);

impl Drop for Socket {
    fn drop(&mut self) {
        remove_socket(&self.0);
    }
}

/// `path` removed when it is a socket (never anything else).
fn remove_socket(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
            let _ = std::fs::remove_file(path);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// `from`'s directories and regular files copied into `to` (made here).
pub fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let t = e.file_type()?;
        let dest = to.join(e.file_name());
        if t.is_dir() {
            copy_tree(&e.path(), &dest)?;
        } else if t.is_file() {
            std::fs::copy(e.path(), &dest)?;
        }
    }
    Ok(())
}

/// Whether the process `pid` runs (or is a zombie not yet reaped).
#[cfg(unix)]
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 checks for the process and sends nothing.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    true
}

/// The pid a directory's name `lo-<pid>-…` holds; `None` for another name
/// (an older build's `lo-…`, which is swept as one whose process is gone).
fn pid_of(name: &str) -> Option<u32> {
    let rest = PREFIXES.iter().find_map(|p| name.strip_prefix(p))?;
    rest.split('-').next()?.parse().ok()
}

/// The directories (and their sockets) under `root` of gateways that no
/// longer run, removed by this process's first run there. Only lmgw's own
/// prefix in lmgw's own directory is touched; the system's temporary
/// directory is never swept.
pub fn sweep_once(root: &Path) {
    static SWEPT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if root == std::env::temp_dir() {
        return;
    }
    SWEPT.get_or_init(|| sweep(root));
}

fn sweep(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let me = std::process::id();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let ours = PREFIXES.iter().any(|p| name.starts_with(p));
        if !ours || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if pid_of(&name).is_some_and(|p| p == me || alive(p)) {
            continue;
        }
        if name.starts_with(RUN_PREFIX) {
            remove_socket(&socket_path(&e.path().join("profile")));
        }
        let _ = std::fs::remove_dir_all(e.path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash LibreOffice 26.8.2.2 named for a profile (seen 2026-11-19
    /// in its `bind` call): MD5 over the URL's UTF-16 bytes, hex without
    /// leading zeros.
    #[test]
    fn the_socket_is_named_as_libreoffice_names_it() {
        let p = Path::new("/srv/example/lo-1-abc/profile");
        let url = profile_url(p);
        assert_eq!(url, "file:///srv/example/lo-1-abc/profile");
        let name = socket_path(p);
        let name = name.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("OSL_PIPE_"), "{name}");
        assert!(name.contains("_SingleOfficeIPC_"), "{name}");
        // A byte below 0x10 is one hex digit.
        let digest = md5::Md5::digest(
            "file:///srv/example/lo-1-abc/profile"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<u8>>(),
        );
        let hex: String = digest.iter().map(|b| format!("{b:x}")).collect();
        assert!(name.ends_with(&hex), "{name}");
    }

    #[cfg(unix)]
    #[test]
    fn the_template_s_name_is_not_the_pid_alone_and_a_socket_there_is_taken() {
        let root = Path::new("/srv/example");
        let a = template_dir(root);
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        let random = name
            .strip_prefix(&format!("{}profile-", own_prefix()))
            .unwrap();
        assert_eq!(random.len(), 12, "{name}");
        // One per process: every run copies the same template.
        assert_eq!(template_dir(root), a);
        // The sweep still reads its pid.
        assert_eq!(pid_of(&name), Some(std::process::id()));
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("OSL_PIPE_example");
        assert!(free(&sock));
        assert!(!foreign(&sock));
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(!free(&sock));
        // Ours, not another user's.
        assert!(!foreign(&sock));
    }

    #[test]
    fn only_dead_gateways_directories_are_swept() {
        let t = tempfile::tempdir().unwrap();
        let mine = t.path().join(format!("{}abc", own_prefix()));
        // pid 1 runs; a pid past the kernel's maximum never does.
        let other = t.path().join("lo-1-def");
        let dead = t.path().join("lo-4194999-ghi");
        let old = t.path().join("lo-xyz");
        let ffmpeg = t.path().join("ff-4194999-jkl");
        let foreign = t.path().join("keep-me");
        for d in [&mine, &other, &dead, &old, &foreign, &ffmpeg] {
            std::fs::create_dir(d).unwrap();
        }
        sweep(t.path());
        assert!(mine.exists() && other.exists() && foreign.exists());
        assert!(!dead.exists() && !old.exists() && !ffmpeg.exists());
    }
}
