//! The one seam every fact goes through (machine-check design §6): files,
//! links, directory listings, the environment and the programs the check runs.
//!
//! [`SystemFacts`] reads this machine. `fake::FakeFacts` (the `fake` feature,
//! for tests) is a machine a test states in memory, so the readers in this
//! crate never touch the computer the suite runs on.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(unix)]
#[path = "facts/unix.rs"]
mod sys;
#[cfg(windows)]
#[path = "facts/windows.rs"]
mod sys;

/// What a program printed and how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CmdOut {
    /// The exit code; `None` when a signal ended it.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOut {
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }

    /// An exit-0 answer that printed `stdout`.
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            status: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// A failed answer: exit `status`, `stderr` printed.
    pub fn failure(status: i32, stderr: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    /// `status` as people read it: the code, or "a signal".
    pub fn status_text(&self) -> String {
        match self.status {
            Some(code) => code.to_string(),
            None => "killed by a signal".to_string(),
        }
    }
}

/// What kind of file a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// The parts of a `stat` the readers use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    /// The POSIX mode and owner; `None` on a system that has neither
    /// (Windows, whose owner and access are a SID and a DACL). A reader that
    /// checks them treats `None` as a file that does not pass, never as one
    /// that does.
    pub posix: Option<Posix>,
    pub len: u64,
    /// The inode; on Windows the NTFS file ID (the volume's 64-bit file
    /// index), which is as stable and as unique within a volume.
    pub ino: u64,
    /// Modification time, nanoseconds since the epoch.
    pub mtime_ns: i128,
    /// Status change time, nanoseconds since the epoch: when the file was
    /// last written, renamed or had its times set. rpm sets an installed
    /// file's modification time to the one in the package, so this is when
    /// the package put it there. On Windows NTFS's change time, which moves
    /// on the same events.
    pub ctime_ns: i128,
}

/// A file's POSIX mode and owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posix {
    /// The permission bits and the setuid, setgid and sticky bits.
    pub mode: u32,
    pub uid: u32,
}

/// A file system's room, as `statvfs` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    /// What an unprivileged user may still write (`f_bavail`).
    pub free_bytes: u64,
    pub total_bytes: u64,
    /// The file system's ID (`f_fsid`): two paths with one ID are on one
    /// file system.
    pub fs_id: u64,
}

/// Everything the machine check reads, behind one trait.
///
/// Synchronous on purpose: every fact is a file read, a `stat` or a short
/// program run, and the helper that will share this crate (machine-check
/// design §3.1) is a small synchronous binary. An async caller runs a reader
/// on a blocking thread.
pub trait Facts: Send + Sync {
    /// The whole file.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// `stat`, following symlinks.
    fn stat(&self, path: &Path) -> io::Result<Meta>;
    /// `lstat`: a symlink is reported as one.
    fn lstat(&self, path: &Path) -> io::Result<Meta>;
    /// One level of a symlink.
    fn read_link(&self, path: &Path) -> io::Result<PathBuf>;
    /// Every symlink resolved, as `realpath`.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    /// The names in a directory, sorted.
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;
    /// An environment variable of the process that reads.
    fn env(&self, name: &str) -> Option<String>;
    /// The real user ID of the process that reads; `None` on a system
    /// without numeric user IDs (Windows).
    fn uid(&self) -> Option<u32>;
    /// That user's name, as the system's user database gives it
    /// (`getpwuid`: `/etc/passwd`, and SSSD's or LDAP's accounts too, which
    /// are not in that file); `None` when it has no entry.
    fn user_name(&self) -> Option<String>;
    /// Run `program` (an absolute path, never looked up in `PATH`) with
    /// `args`, in the C locale, with no input, and wait for it.
    fn run(&self, program: &Path, args: &[&str]) -> io::Result<CmdOut>;
    /// [`Self::run`], stopped when `limit` runs out: the program and every
    /// process it started in its process group are killed (on Windows only
    /// the program itself, not yet what it started), and the answer is
    /// an error of kind [`io::ErrorKind::TimedOut`]. `limit` is the caller's,
    /// a bound the person can see and set (lmgw's model start passes its load
    /// timeout); this seam invents none.
    fn run_limited(&self, program: &Path, args: &[&str], limit: Duration) -> io::Result<CmdOut>;
    /// `statvfs` of the file system `path` is on.
    fn space(&self, path: &Path) -> io::Result<Space>;
    /// This machine as root's environment reads it (machine-check design
    /// §3.1, D60, D87): every program from [`ROOT_ENV`] alone, in `/`, and
    /// [`Self::env`] answering from that list, while still running as
    /// whoever reads. The plan's inputs are read through it on both sides
    /// ([`crate::plan::Inputs::read`]), so nothing the user may set (a
    /// `~/.rpmmacros` that moves rpm's `%_dbpath`, `%_keyring` or
    /// `%_keyringpath`, `RPM_CONFIGDIR`) makes the user's plan hash differ
    /// from root's; only what root may read and the user may not still can.
    /// `None`: these reads are root's environment's already (root's own,
    /// [`SystemFacts::for_root`], and a machine that runs no program).
    fn root_env(&self) -> Option<Arc<dyn Facts>> {
        None
    }
}

/// Whether `path` exists (following symlinks).
pub fn exists(f: &dyn Facts, path: &Path) -> bool {
    f.stat(path).is_ok()
}

/// The file as text, lossily.
pub fn read_text(f: &dyn Facts, path: &Path) -> io::Result<String> {
    f.read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// What a caller does to a command before [`SystemFacts`] spawns it — lmgw
/// strips inherited descriptors and the loader path from every system tool it
/// runs (`lmgw_core::child::system_tool`), and hands that in here.
pub type Prepare = fn(&mut Command) -> &mut Command;

/// This machine.
#[derive(Clone, Copy)]
pub struct SystemFacts {
    prepare: Option<Prepare>,
    /// Root's reads ([`Self::for_root`]): no inherited environment at all.
    root: bool,
}

impl Default for SystemFacts {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemFacts {
    pub fn new() -> Self {
        Self {
            prepare: None,
            root: false,
        }
    }

    /// Every command goes through `prepare` before it is spawned.
    pub fn prepared_by(prepare: Prepare) -> Self {
        Self {
            prepare: Some(prepare),
            root: false,
        }
    }

    /// This machine as root reads it for the plan (machine-check design
    /// §3.1, WP6): every program starts from an empty environment with only
    /// [`ROOT_ENV`] set, in `/`, and [`Facts::env`] answers from that same
    /// fixed list, never from the environment root's process inherited. A
    /// variable the user could set — `HOME`, whose `~/.rpmmacros` can move
    /// rpm's `%_dbpath`, `PATH`, `RPM_CONFIGDIR`, a locale — never changes
    /// what root reads, so root's plan is computed from the same facts
    /// whoever started it. The helper must read through this constructor;
    /// `prepare` runs after the environment is cleared, as for
    /// [`Self::prepared_by`]. Run as a user, it is the same reads with the
    /// user's rights ([`Facts::root_env`], D87).
    pub fn for_root(prepare: Option<Prepare>) -> Self {
        Self {
            prepare,
            root: true,
        }
    }
}

/// The whole environment of a program root's facts run
/// ([`SystemFacts::for_root`]).
pub const ROOT_ENV: [(&str, &str); 3] = [
    ("HOME", "/root"),
    ("PATH", "/usr/sbin:/usr/bin"),
    ("LC_ALL", "C.UTF-8"),
];

/// The environment variables `nvidia-ctk` takes its options from
/// (`NVIDIA_CTK_CONFIG`, `NVIDIA_CTK_CDI_HOOK_PATH`, …, every one of them
/// named in `nvidia-ctk --help`): none reaches a program run here, so what it
/// does is what its arguments say, not what the caller's environment happens
/// to hold.
pub const NVIDIA_CTK_ENV_PREFIX: &str = "NVIDIA_CTK_";

/// How often [`SystemFacts::run_limited`] looks whether its program ended: a
/// sampling rate, not a bound.
const EXIT_POLL: Duration = Duration::from_millis(10);

impl Facts for SystemFacts {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn stat(&self, path: &Path) -> io::Result<Meta> {
        sys::stat(path)
    }

    fn lstat(&self, path: &Path) -> io::Result<Meta> {
        sys::lstat(path)
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::read_link(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut names: Vec<String> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        Ok(names)
    }

    fn env(&self, name: &str) -> Option<String> {
        if self.root {
            return ROOT_ENV
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_string());
        }
        std::env::var(name).ok()
    }

    fn uid(&self) -> Option<u32> {
        sys::uid()
    }

    fn user_name(&self) -> Option<String> {
        sys::user_name()
    }

    fn run(&self, program: &Path, args: &[&str]) -> io::Result<CmdOut> {
        let out = self.command(program, args).output()?;
        Ok(CmdOut {
            status: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn run_limited(&self, program: &Path, args: &[&str], limit: Duration) -> io::Result<CmdOut> {
        let started = Instant::now();
        let deadline = started + limit;
        let mut child = sys::spawn_killable(&mut self.command(program, args))?;
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(CmdOut {
                    status: status.code(),
                    stdout: joined(stdout),
                    stderr: joined(stderr),
                });
            }
            let now = Instant::now();
            if now >= deadline {
                sys::kill_all(child);
                // The pipes' readers are left to end on their own: a process
                // that left the group and holds a pipe must not hold this
                // caller past its limit too.
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{} ran for {:.1} s without finishing and was stopped",
                        program.display(),
                        started.elapsed().as_secs_f64()
                    ),
                ));
            }
            std::thread::sleep(EXIT_POLL.min(deadline - now));
        }
    }

    /// A path not made yet is on the file system of its nearest ancestor
    /// that is there: where it would be made.
    fn space(&self, path: &Path) -> io::Result<Space> {
        let mut first_error = None;
        for at in path.ancestors() {
            match sys::space(at) {
                Ok(space) => return Ok(space),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    first_error.get_or_insert(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(first_error.unwrap_or_else(|| io::Error::from(io::ErrorKind::NotFound)))
    }

    /// The same command hygiene (`prepare`), with the environment and the
    /// working directory of [`Self::for_root`].
    fn root_env(&self) -> Option<Arc<dyn Facts>> {
        (!self.root).then(|| Arc::new(Self::for_root(self.prepare)) as Arc<dyn Facts>)
    }
}

impl SystemFacts {
    /// `program` with `args` as every run here starts it: the C locale, no
    /// input, both outputs piped, no `nvidia-ctk` option from the
    /// environment ([`NVIDIA_CTK_ENV_PREFIX`]), then the caller's hygiene.
    /// Root's ([`Self::for_root`]) starts from an empty environment with only
    /// [`ROOT_ENV`], in `/`: pkexec with `--keep-cwd` keeps the working
    /// directory of whoever asked, and a program that reads a relative path
    /// must not find that person's files there.
    fn command(&self, program: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if self.root {
            cmd.env_clear().envs(ROOT_ENV).current_dir("/");
            if let Some(prepare) = self.prepare {
                prepare(&mut cmd);
            }
            return cmd;
        }
        cmd.env("LC_ALL", "C.UTF-8");
        for (name, _) in std::env::vars_os() {
            if name
                .to_str()
                .is_some_and(|n| n.starts_with(NVIDIA_CTK_ENV_PREFIX))
            {
                cmd.env_remove(name);
            }
        }
        if let Some(prepare) = self.prepare {
            prepare(&mut cmd);
        }
        cmd
    }
}

/// A pipe read to its end on a thread of its own, so a program that fills
/// one pipe while the other is not read cannot stall.
fn drain<R: io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

fn joined(reader: std::thread::JoinHandle<Vec<u8>>) -> String {
    String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned()
}

// Windows' file facts and free space (facts/windows.rs).
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn stat_and_lstat_read_kind_size_id_and_times() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"twelve bytes").unwrap();
        let f = SystemFacts::new();
        let m = f.stat(&file).unwrap();
        assert_eq!((m.kind, m.len, m.posix), (Kind::File, 12, None));
        assert_ne!(m.ino, 0);
        let modified = std::fs::metadata(&file)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i128;
        assert_eq!(m.mtime_ns, modified);
        assert!(m.ctime_ns >= m.mtime_ns);
        assert_eq!(f.lstat(&file).unwrap(), m);
        assert_eq!(f.stat(dir.path()).unwrap().kind, Kind::Dir);
        // A junction is a link to lstat and its target to stat.
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("j");
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(f.lstat(&link).unwrap().kind, Kind::Symlink);
        // Followed, it is the directory; or Windows' redirection guard
        // refuses to traverse a junction a standard user made
        // (ERROR_UNTRUSTED_MOUNT_POINT, 448: seen on Windows 11 build 26100).
        match f.stat(&link) {
            Ok(through) => {
                assert_eq!(through.kind, Kind::Dir);
                assert_eq!(through.ino, f.stat(&real).unwrap().ino);
            }
            Err(e) => assert_eq!(
                e.raw_os_error(),
                Some(448),
                "stat through the junction: {e}"
            ),
        }
        let e = f.stat(&dir.path().join("none")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    /// A path not made yet is on its nearest existing ancestor's volume.
    #[test]
    fn the_space_of_a_path_not_made_yet_is_its_parents() {
        let dir = std::env::temp_dir();
        let f = SystemFacts::new();
        let here = f.space(&dir).unwrap();
        let later = f.space(&dir.join("not-made-yet/deeper")).unwrap();
        assert_eq!(here.fs_id, later.fs_id);
        assert!(here.total_bytes > 0 && here.free_bytes <= here.total_bytes);
    }
}

// The system's own programs (`/bin/sh`, `/usr/bin/id`) and its POSIX facts.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_limited_run_gives_both_outputs_and_the_exit() {
        let out = SystemFacts::new()
            .run_limited(
                Path::new("/bin/sh"),
                &["-c", "echo out; echo err >&2; exit 3"],
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(out.status, Some(3));
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
    }

    /// A path not made yet is on its nearest existing ancestor's file system.
    #[test]
    fn the_space_of_a_path_not_made_yet_is_its_parents() {
        let dir = std::env::temp_dir();
        let f = SystemFacts::new();
        let here = f.space(&dir).unwrap();
        let later = f.space(&dir.join("not-made-yet/deeper")).unwrap();
        assert_eq!(here.fs_id, later.fs_id);
        assert!(here.total_bytes > 0);
    }

    /// The user's name comes from the user database, as `id -un` gives it.
    #[test]
    fn the_user_name_is_the_user_databases() {
        let id = std::process::Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .unwrap();
        let expected = String::from_utf8_lossy(&id.stdout).trim().to_string();
        assert_eq!(SystemFacts::new().user_name(), Some(expected));
    }

    /// Root's reads start every program from [`ROOT_ENV`] alone, and its
    /// `env` answers from that list: nothing the starting process inherited
    /// (a `HOME` whose `.rpmmacros` moves rpm's database) reaches them.
    #[test]
    fn roots_reads_inherit_no_environment() {
        let f = SystemFacts::for_root(None);
        let out = f.run(Path::new("/usr/bin/env"), &[]).unwrap();
        let mut lines: Vec<&str> = out.stdout.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            ["HOME=/root", "LC_ALL=C.UTF-8", "PATH=/usr/sbin:/usr/bin"],
            "{}",
            out.stdout
        );
        assert_eq!(f.env("HOME").as_deref(), Some("/root"));
        assert_eq!(f.env("XDG_RUNTIME_DIR"), None);
        // The user's reads keep theirs.
        let user = SystemFacts::new()
            .run(Path::new("/usr/bin/env"), &[])
            .unwrap();
        assert!(user.stdout.lines().any(|l| l.starts_with("PATH=")));
        assert!(user.stdout.contains("LC_ALL=C.UTF-8"));
    }

    /// Root's reads run in `/` whatever directory the reading process is
    /// in (pkexec with `--keep-cwd` keeps the caller's).
    #[test]
    fn roots_reads_run_in_the_root_directory() {
        let f = SystemFacts::for_root(None);
        let out = f.run(Path::new("/usr/bin/pwd"), &[]).unwrap();
        assert_eq!(out.stdout, "/\n");
        let out = f
            .run_limited(Path::new("/usr/bin/pwd"), &[], Duration::from_secs(60))
            .unwrap();
        assert_eq!(out.stdout, "/\n");
    }

    /// The user's reads seen through root's environment (D87): the
    /// programs get [`ROOT_ENV`] alone and `/`, `env` answers from that list,
    /// and they still run as the user. Root's own reads are root's
    /// environment already.
    #[test]
    fn the_users_reads_through_roots_environment() {
        let user = SystemFacts::new();
        let root_env = user.root_env().expect("the user's reads have a root view");
        let out = root_env.run(Path::new("/usr/bin/env"), &[]).unwrap();
        let mut lines: Vec<&str> = out.stdout.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            ["HOME=/root", "LC_ALL=C.UTF-8", "PATH=/usr/sbin:/usr/bin"],
            "{}",
            out.stdout
        );
        assert_eq!(root_env.env("HOME").as_deref(), Some("/root"));
        let pwd = root_env.run(Path::new("/usr/bin/pwd"), &[]).unwrap();
        assert_eq!(pwd.stdout, "/\n");
        let id = root_env.run(Path::new("/usr/bin/id"), &["-u"]).unwrap();
        assert_eq!(
            Some(id.stdout.trim().to_string()),
            user.uid().map(|u| u.to_string())
        );
        assert_eq!(root_env.uid(), user.uid());
        assert!(SystemFacts::for_root(None).root_env().is_none());
        assert!(root_env.root_env().is_none());
    }

    /// The program and what it started in the background are stopped at the
    /// limit, though the background one holds the output pipe open.
    #[test]
    fn a_limited_run_that_hangs_is_killed_with_its_group() {
        let started = Instant::now();
        let e = SystemFacts::new()
            .run_limited(
                Path::new("/bin/sh"),
                &["-c", "sleep 30 & sleep 30"],
                Duration::from_millis(200),
            )
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(e.to_string().contains("/bin/sh ran for"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
