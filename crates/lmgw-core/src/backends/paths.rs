//! Where builds keep their files, and whether that place can hold them
//! (container-builds design §5 "Workspace", §10 "Dev instance").
//!
//! A build's working set is a bare mirror per remote plus a full worktree per
//! run: gigabytes that must not live in RAM. `/tmp` is a 32 GB tmpfs on this
//! host (§2.3) and a dev instance's data dir sits on it, so the default a dev
//! instance gets is under `~/.cache` instead, and a run refuses to start on a
//! tmpfs with [`tmpfs_refusal`]'s sentence.

use std::path::{Path, PathBuf};

use crate::config::Settings;

/// The directory builds use: the `builds_dir` setting when set, else
/// `<data_dir>/builds` — or, on a dev instance, [`dev_builds_dir`], because a
/// dev data dir is a `mktemp` directory on the `/tmp` tmpfs.
pub fn builds_dir(settings: &Settings, data_dir: &Path, dev: bool) -> PathBuf {
    match settings
        .builds_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(dir) => PathBuf::from(dir),
        None if dev => dev_builds_dir(),
        None => data_dir.join("builds"),
    }
}

/// `$XDG_CACHE_HOME/lmgw-dev/builds`, else `~/.cache/lmgw-dev/builds`: off the
/// tmpfs, shared by every dev instance (their runs are serialized by the same
/// machine-wide lock as prod's), and never the production data dir.
pub fn dev_builds_dir() -> PathBuf {
    let cache = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|x| Path::new(x).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".cache")
        });
    cache.join("lmgw-dev").join("builds")
}

// ---------------------------------------------------------------------------
// A run's files
// ---------------------------------------------------------------------------
//
// Every per-run name carries the instance id as well as the run id: run ids
// are per database, the builds dir is not — every dev instance shares
// `~/.cache/lmgw-dev/builds` and counts its runs from 1, and a dev copy of
// production's database may be pointed at production's builds dir. With the
// instance in the name, two instances' run 7 can never share a worktree, a
// build context, a scratch dir or a log, and the boot sweep can tell its own
// leftovers from another instance's.

/// `run-<instance>-<run id>`: a run's worktree under `<builds_dir>/work`, and
/// (with [`CONTEXT_SUFFIX`]) its build context beside it, and its scratch dir
/// under `<builds_dir>/tmp`.
pub fn run_dir_name(instance: &str, run_id: i64) -> String {
    format!("run-{instance}-{run_id}")
}

/// The suffix of a run's build context directory (`run-<i>-<id>.ctx`).
pub const CONTEXT_SUFFIX: &str = ".ctx";

/// `<builds_dir>/work/run-<instance>-<run id>`: the run's worktree.
pub fn worktree_dir(builds_dir: &Path, instance: &str, run_id: i64) -> PathBuf {
    builds_dir.join("work").join(run_dir_name(instance, run_id))
}

/// `<builds_dir>/work/run-<instance>-<run id>.ctx`: the Containerfile copy,
/// ignore file and iidfile — outside the tree it builds.
pub fn context_dir(builds_dir: &Path, instance: &str, run_id: i64) -> PathBuf {
    builds_dir.join("work").join(format!(
        "{}{CONTEXT_SUFFIX}",
        run_dir_name(instance, run_id)
    ))
}

/// `<builds_dir>/tmp/run-<instance>-<run id>`: the `TMPDIR` of the run's
/// `podman build` — where buildah puts its `buildahNNN` scratch directory and
/// its blob downloads, so what an interrupted build leaves is exactly this
/// directory's contents, never something of another build's in `/var/tmp`.
pub fn run_tmp_dir(builds_dir: &Path, instance: &str, run_id: i64) -> PathBuf {
    builds_dir.join("tmp").join(run_dir_name(instance, run_id))
}

/// `<builds_dir>/logs/<instance>-<run id>.log`: the run's log.
pub fn log_path(builds_dir: &Path, instance: &str, run_id: i64) -> PathBuf {
    builds_dir
        .join("logs")
        .join(format!("{instance}-{run_id}.log"))
}

/// Whether `name` (an entry of `<builds_dir>/work` or `<builds_dir>/tmp`) is
/// one of `instance`'s run directories — `run-<instance>-<digits>`, with or
/// without [`CONTEXT_SUFFIX`] — and not another instance's.
pub fn is_own_run_dir(name: &str, instance: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("run-")
        .and_then(|r| r.strip_prefix(instance))
        .and_then(|r| r.strip_prefix('-'))
    else {
        return false;
    };
    let id = rest.strip_suffix(CONTEXT_SUFFIX).unwrap_or(rest);
    !instance.is_empty() && !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())
}

/// The `builds_dir` setting as typed: blank clears it back to the default, a
/// value must be an absolute path (a relative one would depend on whatever
/// directory lmgw happened to be started from) and cannot be `/`.
pub fn validate_builds_dir(raw: &str) -> Result<Option<String>, String> {
    let v = raw.trim();
    if v.is_empty() {
        return Ok(None);
    }
    if v.chars().any(char::is_control) {
        return Err(format!("builds_dir '{v}' contains a control character"));
    }
    if !Path::new(v).is_absolute() {
        return Err(format!(
            "builds_dir '{v}' is relative; give an absolute path (leave it empty for the \
             default)"
        ));
    }
    if Path::new(v).parent().is_none() {
        return Err(
            "builds_dir cannot be / — builds create and remove directories under it".into(),
        );
    }
    Ok(Some(v.to_string()))
}

/// Whether `path` lives in RAM: on a `tmpfs` (or `ramfs`) mount. A directory
/// that does not exist yet is judged by its nearest existing ancestor, which
/// is the filesystem it would be created on.
///
/// `false` whenever the filesystem cannot be asked — this is a guard that
/// names a problem, and an unanswerable question is not evidence of one.
pub fn is_tmpfs(path: &Path) -> bool {
    path.ancestors()
        .find_map(|p| fs_magic(p).ok())
        .is_some_and(|magic| magic == TMPFS_MAGIC || magic == RAMFS_MAGIC)
}

/// The refusal a run gives when its builds dir is in RAM (§10), naming the
/// setting that moves it. `None` when it is not.
pub fn tmpfs_refusal(path: &Path) -> Option<String> {
    is_tmpfs(path).then(|| {
        format!(
            "the builds directory {} is on tmpfs, i.e. in RAM — a build keeps git mirrors and a \
             full worktree per run there (gigabytes), so it refuses to run on it. Point \
             builds_dir (Settings) at a disk-backed directory, e.g. one under your home",
            path.display()
        )
    })
}

/// Bytes free for an unprivileged writer on the filesystem `path` is (or
/// would be created) on — `statvfs(2)`'s `f_bavail × f_frsize`, judged by the
/// nearest existing ancestor as [`is_tmpfs`] is. `None` when no ancestor can
/// be asked. Shown next to a run's footprint (§9.1): a warning, never a
/// block.
pub fn free_bytes(path: &Path) -> Option<u64> {
    path.ancestors().find_map(|p| statvfs_free(p).ok())
}

#[cfg(unix)]
fn statvfs_free(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: as in `fs_magic` — `statvfs` only writes into the zeroed
    // plain-old-data struct we hand it, and `c` outlives the call.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut buf) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Both fields' widths are per-platform.
    #[allow(clippy::unnecessary_cast)]
    Ok((buf.f_bavail as u64).saturating_mul(buf.f_frsize as u64))
}

#[cfg(not(unix))]
fn statvfs_free(_path: &Path) -> std::io::Result<u64> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "free space is read with statvfs",
    ))
}

// `f_type` values from linux/magic.h.
const TMPFS_MAGIC: i64 = 0x0102_1994;
const RAMFS_MAGIC: i64 = 0x8584_58f6;

/// `statfs(2)`'s `f_type` for `path`.
#[cfg(target_os = "linux")]
fn fs_magic(path: &Path) -> std::io::Result<i64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `statfs` only writes into the struct we hand it, which is a
    // plain-old-data C struct for which all-zero is a valid value, and `c` is
    // a NUL-terminated string that outlives the call.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c.as_ptr(), &mut buf) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // `f_type` is `__fsword_t`, whose width is per-architecture; the magic
    // numbers all fit 32 bits.
    #[allow(clippy::unnecessary_cast)]
    Ok(buf.f_type as i64)
}

#[cfg(not(target_os = "linux"))]
fn fs_magic(_path: &Path) -> std::io::Result<i64> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "filesystem type is read with Linux statfs",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_wins_then_dev_then_the_data_dir() {
        let data = Path::new("/home/u/.local/share/lmgw");
        let mut s = Settings::default();
        assert_eq!(builds_dir(&s, data, false), data.join("builds"));
        assert_eq!(builds_dir(&s, data, true), dev_builds_dir());
        assert!(dev_builds_dir().ends_with("lmgw-dev/builds"));
        s.builds_dir = Some("  ".into());
        assert_eq!(builds_dir(&s, data, false), data.join("builds"));
        s.builds_dir = Some("/srv/builds".into());
        assert_eq!(builds_dir(&s, data, false), PathBuf::from("/srv/builds"));
        assert_eq!(builds_dir(&s, data, true), PathBuf::from("/srv/builds"));
    }

    #[test]
    fn a_runs_files_carry_its_instance_and_id() {
        let b = Path::new("/b");
        assert_eq!(
            worktree_dir(b, "ab12cd34", 7),
            Path::new("/b/work/run-ab12cd34-7")
        );
        assert_eq!(
            context_dir(b, "ab12cd34", 7),
            Path::new("/b/work/run-ab12cd34-7.ctx")
        );
        assert_eq!(
            run_tmp_dir(b, "ab12cd34", 7),
            Path::new("/b/tmp/run-ab12cd34-7")
        );
        assert_eq!(
            log_path(b, "ab12cd34", 7),
            Path::new("/b/logs/ab12cd34-7.log")
        );
        assert!(is_own_run_dir("run-ab12cd34-7", "ab12cd34"));
        assert!(is_own_run_dir("run-ab12cd34-7.ctx", "ab12cd34"));
        assert!(
            !is_own_run_dir("run-ffffffff-7", "ab12cd34"),
            "another instance's"
        );
        assert!(
            !is_own_run_dir("7", "ab12cd34"),
            "an old-format dir: whose is unknown"
        );
        assert!(!is_own_run_dir("run-ab12cd34-", "ab12cd34"));
        assert!(!is_own_run_dir("run-ab12cd34-7x", "ab12cd34"));
        assert!(!is_own_run_dir("run--7", ""));
    }

    #[test]
    fn a_builds_dir_is_absolute_or_blank() {
        assert_eq!(validate_builds_dir(" "), Ok(None));
        assert_eq!(
            validate_builds_dir(" /srv/builds ").unwrap().as_deref(),
            Some("/srv/builds")
        );
        assert!(validate_builds_dir("builds").is_err());
        assert!(validate_builds_dir("/").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn free_space_is_read_from_the_nearest_existing_ancestor() {
        let tmp = std::env::temp_dir();
        let here = free_bytes(&tmp).expect("the temp dir's filesystem answers statvfs");
        assert_eq!(
            free_bytes(&tmp.join("lmgw-no-such-dir/builds")).map(|b| b > 0),
            Some(here > 0),
            "a missing dir is judged by the filesystem it would be created on"
        );
    }

    /// Against the real mounts of the box the suite runs on: `/proc` is never
    /// a tmpfs, and `/dev/shm` is one on every Linux with glibc's POSIX shm.
    /// Skipped quietly where `/dev/shm` is not a tmpfs (some containers).
    #[cfg(target_os = "linux")]
    #[test]
    fn tmpfs_is_told_apart_from_a_disk() {
        assert!(!is_tmpfs(Path::new("/proc")));
        let shm_is_tmpfs = std::fs::read_to_string("/proc/mounts")
            .unwrap_or_default()
            .lines()
            .any(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                f.get(1) == Some(&"/dev/shm") && f.get(2) == Some(&"tmpfs")
            });
        if shm_is_tmpfs {
            assert!(is_tmpfs(Path::new("/dev/shm")));
            // Not there yet: judged by the ancestor it would be created in.
            assert!(is_tmpfs(Path::new("/dev/shm/lmgw-no-such/builds")));
            assert!(tmpfs_refusal(Path::new("/dev/shm/x"))
                .unwrap()
                .contains("builds_dir"));
        }
        assert_eq!(tmpfs_refusal(Path::new("/proc")), None);
    }
}
