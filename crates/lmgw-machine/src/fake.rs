//! A machine a test states in memory (machine-check design §6): files,
//! directories, symlinks, the environment and the programs it has, each
//! program as a closure over its arguments. Every program run is recorded.
//!
//! Interior mutability throughout, so a test can change the machine between
//! two reads — the libraries on disk moving to a new driver version while the
//! kernel module stays, say — through the same shared handle.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::facts::{CmdOut, Facts, Kind, Meta, Posix, Space};

/// A program's behaviour: its arguments in, its answer out.
pub type Handler = Arc<dyn Fn(&[String]) -> io::Result<CmdOut> + Send + Sync>;

/// One program run, as the fake saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub program: PathBuf,
    pub args: Vec<String>,
}

#[derive(Clone)]
enum Node {
    File {
        bytes: Vec<u8>,
        mode: u32,
        uid: u32,
        mtime_ns: i128,
        ctime_ns: i128,
        ino: u64,
    },
    Dir {
        mode: u32,
        uid: u32,
        mtime_ns: i128,
        ctime_ns: i128,
        ino: u64,
    },
    Link {
        target: PathBuf,
        ino: u64,
    },
}

#[derive(Default)]
struct State {
    nodes: BTreeMap<PathBuf, Node>,
    env: BTreeMap<String, String>,
    uid: u32,
    user_name: Option<String>,
    programs: BTreeMap<PathBuf, Handler>,
    /// Programs that answer otherwise in the reading process's own
    /// environment than in root's ([`FakeFacts::program_in_users_env`]).
    users_env_programs: BTreeMap<PathBuf, Handler>,
    /// File systems by mount point: a path's is its nearest ancestor's.
    spaces: BTreeMap<PathBuf, Space>,
    calls: Vec<Call>,
    next_ino: u64,
}

/// The mtime every node gets unless a test sets one: 2026-01-01 00:00 UTC.
pub const DEFAULT_MTIME_NS: i128 = 1_767_225_600 * 1_000_000_000;

/// The kernel's own bound on symlinks followed in one lookup (`MAXSYMLINKS`),
/// past which a lookup is `ELOOP` — the fake answers the same.
const MAX_LINK_HOPS: usize = 40;

/// A machine in memory. See the module doc.
#[derive(Default)]
pub struct FakeFacts {
    st: Arc<Mutex<State>>,
    /// This handle is the machine seen through root's environment
    /// ([`Facts::root_env`]): the same state, root's fixed environment.
    root_env: bool,
}

/// `path` with `.` dropped and `..` applied, always absolute.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{}: No such file or directory", path.display()),
    )
}

impl FakeFacts {
    pub fn new() -> Self {
        Self {
            st: Arc::new(Mutex::new(State {
                uid: 1000,
                next_ino: 1,
                ..State::default()
            })),
            root_env: false,
        }
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn ino(st: &mut State) -> u64 {
        st.next_ino += 1;
        st.next_ino
    }

    /// Every ancestor of `path` as a directory, where none is there yet.
    fn parents(st: &mut State, path: &Path) {
        let mut ancestors: Vec<PathBuf> = path.ancestors().skip(1).map(Path::to_path_buf).collect();
        ancestors.reverse();
        for a in ancestors {
            if !st.nodes.contains_key(&a) {
                let ino = Self::ino(st);
                st.nodes.insert(
                    a,
                    Node::Dir {
                        mode: 0o755,
                        uid: 0,
                        mtime_ns: DEFAULT_MTIME_NS,
                        ctime_ns: DEFAULT_MTIME_NS,
                        ino,
                    },
                );
            }
        }
    }

    /// A root-owned 0644 file with `content`.
    pub fn file(&self, path: impl AsRef<Path>, content: impl AsRef<[u8]>) -> &Self {
        self.file_mode(path, content, 0o644)
    }

    /// A root-owned file with `content` and `mode`.
    pub fn file_mode(&self, path: impl AsRef<Path>, content: impl AsRef<[u8]>, mode: u32) -> &Self {
        let path = normalize(path.as_ref());
        let mut st = self.st();
        Self::parents(&mut st, &path);
        let ino = Self::ino(&mut st);
        st.nodes.insert(
            path,
            Node::File {
                bytes: content.as_ref().to_vec(),
                mode,
                uid: 0,
                mtime_ns: DEFAULT_MTIME_NS,
                ctime_ns: DEFAULT_MTIME_NS,
                ino,
            },
        );
        self
    }

    /// An empty root-owned 0755 directory.
    pub fn dir(&self, path: impl AsRef<Path>) -> &Self {
        let path = normalize(path.as_ref());
        let mut st = self.st();
        Self::parents(&mut st, &path);
        let ino = Self::ino(&mut st);
        st.nodes.entry(path).or_insert(Node::Dir {
            mode: 0o755,
            uid: 0,
            mtime_ns: DEFAULT_MTIME_NS,
            ctime_ns: DEFAULT_MTIME_NS,
            ino,
        });
        self
    }

    /// A symlink at `path` pointing at `target` (relative to `path`'s
    /// directory unless absolute), as `ln -s target path` makes it.
    pub fn link(&self, path: impl AsRef<Path>, target: impl AsRef<Path>) -> &Self {
        let path = normalize(path.as_ref());
        let mut st = self.st();
        Self::parents(&mut st, &path);
        let ino = Self::ino(&mut st);
        st.nodes.insert(
            path,
            Node::Link {
                target: target.as_ref().to_path_buf(),
                ino,
            },
        );
        self
    }

    /// `path` and everything under it gone.
    pub fn remove(&self, path: impl AsRef<Path>) -> &Self {
        let path = normalize(path.as_ref());
        self.st()
            .nodes
            .retain(|p, _| p != &path && !p.starts_with(&path));
        self
    }

    /// Set the modification time of the file or directory at `path` (not
    /// following a symlink), and its change time with it. A path that is not
    /// there is left alone.
    pub fn touch(&self, path: impl AsRef<Path>, mtime_ns: i128) -> &Self {
        let path = normalize(path.as_ref());
        if let Some(
            Node::File {
                mtime_ns: m,
                ctime_ns: c,
                ..
            }
            | Node::Dir {
                mtime_ns: m,
                ctime_ns: c,
                ..
            },
        ) = self.st().nodes.get_mut(&path)
        {
            *m = mtime_ns;
            *c = mtime_ns;
        }
        self
    }

    /// Set only the change time of the file or directory at `path`, as rpm
    /// leaves it: installed at `ctime_ns`, its modification time the one in
    /// the package.
    pub fn touch_ctime(&self, path: impl AsRef<Path>, ctime_ns: i128) -> &Self {
        let path = normalize(path.as_ref());
        if let Some(Node::File { ctime_ns: c, .. } | Node::Dir { ctime_ns: c, .. }) =
            self.st().nodes.get_mut(&path)
        {
            *c = ctime_ns;
        }
        self
    }

    /// Give the file at `path` a new inode, as replacing it by rename does.
    pub fn replace_inode(&self, path: impl AsRef<Path>) -> &Self {
        let path = normalize(path.as_ref());
        let mut st = self.st();
        let ino = Self::ino(&mut st);
        if let Some(Node::File { ino: i, .. }) = st.nodes.get_mut(&path) {
            *i = ino;
        }
        self
    }

    /// The owner of the file or directory at `path`.
    pub fn chown(&self, path: impl AsRef<Path>, owner: u32) -> &Self {
        let path = normalize(path.as_ref());
        if let Some(Node::File { uid, .. } | Node::Dir { uid, .. }) = self.st().nodes.get_mut(&path)
        {
            *uid = owner;
        }
        self
    }

    pub fn set_env(&self, name: &str, value: impl Into<String>) -> &Self {
        self.st().env.insert(name.to_string(), value.into());
        self
    }

    pub fn unset_env(&self, name: &str) -> &Self {
        self.st().env.remove(name);
        self
    }

    pub fn set_uid(&self, uid: u32) -> &Self {
        self.st().uid = uid;
        self
    }

    /// The user's name in the user database (none until a test sets one).
    pub fn set_user_name(&self, name: &str) -> &Self {
        self.st().user_name = Some(name.to_string());
        self
    }

    /// Install `program` (a 0755 file at its path) behaving as `handler`.
    /// A program installed again behaves as the new handler.
    pub fn program(
        &self,
        program: impl AsRef<Path>,
        handler: impl Fn(&[String]) -> io::Result<CmdOut> + Send + Sync + 'static,
    ) -> &Self {
        let path = normalize(program.as_ref());
        if !self.st().nodes.contains_key(&path) {
            self.file_mode(&path, b"\x7fELF", 0o755);
        }
        self.st().programs.insert(path, Arc::new(handler));
        self
    }

    /// `program` answers as `handler` when the reading process runs it in
    /// its own environment, and as stated ([`Self::program`]) through root's
    /// ([`Facts::root_env`]): rpm with the person's `~/.rpmmacros` moving
    /// its database or its keyring, say.
    pub fn program_in_users_env(
        &self,
        program: impl AsRef<Path>,
        handler: impl Fn(&[String]) -> io::Result<CmdOut> + Send + Sync + 'static,
    ) -> &Self {
        let path = normalize(program.as_ref());
        self.st().users_env_programs.insert(path, Arc::new(handler));
        self
    }

    /// Install `program` answering `out` to every call.
    pub fn reply(&self, program: impl AsRef<Path>, out: CmdOut) -> &Self {
        self.program(program, move |_| Ok(out.clone()))
    }

    /// The program gone: its file and its behaviour.
    pub fn uninstall(&self, program: impl AsRef<Path>) -> &Self {
        let path = normalize(program.as_ref());
        self.st().programs.remove(&path);
        self.st().users_env_programs.remove(&path);
        self.remove(&path)
    }

    /// The open NVIDIA kernel module at `version` running, as
    /// `/proc/driver/nvidia/version` says it.
    pub fn set_kernel_module(&self, version: &str) -> &Self {
        self.file(
            crate::driver::PROC_NVIDIA_VERSION,
            format!(
                "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  {version}  Release \
                 Build  (builder@example)  Tue Oct  6 04:56:08 UTC 2026\nGCC version:  gcc \
                 version 16.2.1 20261029 (Red Hat 16.2.1-3) (GCC) \n"
            ),
        )
    }

    /// The driver's libraries at `version` on disk: `libnvidia-ml.so.1` in
    /// `/usr/lib64` pointing at `libnvidia-ml.so.<version>`. An earlier
    /// version's file stays, as an update leaves nothing else behind here.
    pub fn set_driver_libraries(&self, version: &str) -> &Self {
        let target = format!("libnvidia-ml.so.{version}");
        self.file(format!("/usr/lib64/{target}"), b"\x7fELF")
            .link("/usr/lib64/libnvidia-ml.so.1", &target)
    }

    /// This boot's ID, as `/proc/sys/kernel/random/boot_id` gives it (a
    /// reboot is a new one).
    pub fn set_boot_id(&self, id: &str) -> &Self {
        self.file(crate::cdi::own::BOOT_ID, format!("{id}\n"))
    }

    /// The device nodes one NVIDIA card and its driver have once CUDA ran:
    /// the card, the control node, unified memory and modeset.
    pub const NVIDIA_NODES: [&str; 5] = [
        "/dev/nvidia0",
        "/dev/nvidiactl",
        "/dev/nvidia-uvm",
        "/dev/nvidia-uvm-tools",
        "/dev/nvidia-modeset",
    ];

    /// A machine with NVIDIA's driver at `version` (module and libraries
    /// agree), its device nodes ([`Self::NVIDIA_NODES`]), the toolkit's
    /// `nvidia-ctk` 1.20.1 (its `--version` only; a test that generates
    /// installs its own), an rpm database and a boot ID.
    pub fn nvidia(version: &str) -> Self {
        let f = Self::new();
        f.set_kernel_module(version).set_driver_libraries(version);
        for node in Self::NVIDIA_NODES {
            f.file(node, b"");
        }
        f.reply(
            crate::cdi::own::NVIDIA_CTK,
            CmdOut::success(
                "NVIDIA Container Toolkit CLI version 1.20.1\ncommit: 0123456789abcdef\n",
            ),
        );
        for name in ["rpmdb.sqlite", "rpmdb.sqlite-wal", "rpmdb.sqlite-shm"] {
            f.file(format!("{}/{name}", crate::rpm::RPMDB_DIRS[0]), b"");
        }
        f.set_boot_id("0f6c1e9a-5b7d-4e2a-9c3f-000000000001");
        f
    }

    /// Every program run so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.st().calls.clone()
    }

    /// The runs of `program` so far.
    pub fn calls_of(&self, program: impl AsRef<Path>) -> Vec<Vec<String>> {
        let path = normalize(program.as_ref());
        self.st()
            .calls
            .iter()
            .filter(|c| c.program == path)
            .map(|c| c.args.clone())
            .collect()
    }

    /// `path` with every symlink on the way resolved; the last component's
    /// too when `follow_last`. A missing last component resolves to itself,
    /// a missing directory on the way is `NotFound`.
    fn resolve(st: &State, path: &Path, follow_last: bool) -> io::Result<PathBuf> {
        let mut queue: VecDeque<OsString> = normalize(path)
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n.to_os_string()),
                _ => None,
            })
            .collect();
        let mut out = PathBuf::from("/");
        let mut hops = 0;
        while let Some(c) = queue.pop_front() {
            let next = out.join(&c);
            let last = queue.is_empty();
            match st.nodes.get(&next) {
                Some(Node::Link { target, .. }) if !last || follow_last => {
                    hops += 1;
                    if hops > MAX_LINK_HOPS {
                        return Err(io::Error::other(format!(
                            "{}: Too many levels of symbolic links",
                            path.display()
                        )));
                    }
                    let target = normalize(&out.join(target));
                    let mut rest: VecDeque<OsString> = target
                        .components()
                        .filter_map(|c| match c {
                            Component::Normal(n) => Some(n.to_os_string()),
                            _ => None,
                        })
                        .collect();
                    rest.extend(queue);
                    queue = rest;
                    out = PathBuf::from("/");
                }
                Some(_) => out = next,
                None if last => out = next,
                None => return Err(not_found(path)),
            }
        }
        Ok(out)
    }

    fn meta(node: &Node) -> Meta {
        match node {
            Node::File {
                bytes,
                mode,
                uid,
                mtime_ns,
                ctime_ns,
                ino,
            } => Meta {
                kind: Kind::File,
                posix: Some(Posix {
                    mode: *mode,
                    uid: *uid,
                }),
                len: bytes.len() as u64,
                ino: *ino,
                mtime_ns: *mtime_ns,
                ctime_ns: *ctime_ns,
            },
            Node::Dir {
                mode,
                uid,
                mtime_ns,
                ctime_ns,
                ino,
            } => Meta {
                kind: Kind::Dir,
                posix: Some(Posix {
                    mode: *mode,
                    uid: *uid,
                }),
                len: 0,
                ino: *ino,
                mtime_ns: *mtime_ns,
                ctime_ns: *ctime_ns,
            },
            Node::Link { target, ino } => Meta {
                kind: Kind::Symlink,
                posix: Some(Posix {
                    mode: 0o777,
                    uid: 0,
                }),
                len: target.as_os_str().len() as u64,
                ino: *ino,
                mtime_ns: DEFAULT_MTIME_NS,
                ctime_ns: DEFAULT_MTIME_NS,
            },
        }
    }
}

impl Facts for FakeFacts {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let st = self.st();
        let at = Self::resolve(&st, path, true)?;
        match st.nodes.get(&at) {
            Some(Node::File { bytes, .. }) => Ok(bytes.clone()),
            Some(_) => Err(io::Error::other(format!(
                "{}: Is a directory",
                path.display()
            ))),
            None => Err(not_found(path)),
        }
    }

    fn stat(&self, path: &Path) -> io::Result<Meta> {
        let st = self.st();
        let at = Self::resolve(&st, path, true)?;
        st.nodes
            .get(&at)
            .map(Self::meta)
            .ok_or_else(|| not_found(path))
    }

    fn lstat(&self, path: &Path) -> io::Result<Meta> {
        let st = self.st();
        let at = Self::resolve(&st, path, false)?;
        st.nodes
            .get(&at)
            .map(Self::meta)
            .ok_or_else(|| not_found(path))
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        let st = self.st();
        let at = Self::resolve(&st, path, false)?;
        match st.nodes.get(&at) {
            Some(Node::Link { target, .. }) => Ok(target.clone()),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: Invalid argument", path.display()),
            )),
            None => Err(not_found(path)),
        }
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let st = self.st();
        let at = Self::resolve(&st, path, true)?;
        if st.nodes.contains_key(&at) {
            Ok(at)
        } else {
            Err(not_found(path))
        }
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let st = self.st();
        let at = Self::resolve(&st, dir, true)?;
        match st.nodes.get(&at) {
            Some(Node::Dir { .. }) => {}
            Some(_) => {
                return Err(io::Error::other(format!(
                    "{}: Not a directory",
                    dir.display()
                )))
            }
            None => return Err(not_found(dir)),
        }
        let mut names: Vec<String> = st
            .nodes
            .keys()
            .filter(|p| p.parent() == Some(at.as_path()))
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        names.sort();
        Ok(names)
    }

    /// Through root's environment ([`Facts::root_env`]) the answer is
    /// [`crate::facts::ROOT_ENV`]'s, as for `SystemFacts::for_root`.
    fn env(&self, name: &str) -> Option<String> {
        if self.root_env {
            return crate::facts::ROOT_ENV
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_string());
        }
        self.st().env.get(name).cloned()
    }

    fn uid(&self) -> Option<u32> {
        Some(self.st().uid)
    }

    fn user_name(&self) -> Option<String> {
        self.st().user_name.clone()
    }

    fn run(&self, program: &Path, args: &[&str]) -> io::Result<CmdOut> {
        let (args, handler) = self.recorded(program, args);
        // Called with the lock released: a handler may change the machine
        // (a generator writing its output, an install adding a file).
        match handler {
            Some(h) => h(&args),
            None => Err(not_found(program)),
        }
    }

    /// The handler runs on a thread of its own, and the answer is
    /// [`io::ErrorKind::TimedOut`] when it has not returned within `limit`.
    /// A closure cannot be killed: a handler that outlives its limit runs on
    /// to its end, unobserved, as a killed program's work would be lost.
    fn run_limited(&self, program: &Path, args: &[&str], limit: Duration) -> io::Result<CmdOut> {
        let started = Instant::now();
        let (args, handler) = self.recorded(program, args);
        let Some(h) = handler else {
            return Err(not_found(program));
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(h(&args));
        });
        match rx.recv_timeout(limit) {
            Ok(answer) => answer,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{} ran for {:.1} s without finishing and was stopped",
                    program.display(),
                    started.elapsed().as_secs_f64()
                ),
            )),
        }
    }

    fn space(&self, path: &Path) -> io::Result<Space> {
        let path = normalize(path);
        let st = self.st();
        path.ancestors()
            .find_map(|a| st.spaces.get(a).copied())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{}: no file system stated", path.display()),
                )
            })
    }

    /// The same machine (one state, one record of calls), its programs
    /// answering as stated rather than as [`FakeFacts::program_in_users_env`]
    /// says.
    fn root_env(&self) -> Option<Arc<dyn Facts>> {
        (!self.root_env).then(|| {
            Arc::new(FakeFacts {
                st: self.st.clone(),
                root_env: true,
            }) as Arc<dyn Facts>
        })
    }
}

impl FakeFacts {
    /// A file system mounted at `mount` with `free_bytes` of `total_bytes`
    /// free; `fs_id` tells two apart.
    pub fn set_space(
        &self,
        mount: impl AsRef<Path>,
        free_bytes: u64,
        total_bytes: u64,
        fs_id: u64,
    ) -> &Self {
        self.st().spaces.insert(
            normalize(mount.as_ref()),
            Space {
                free_bytes,
                total_bytes,
                fs_id,
            },
        );
        self
    }

    /// Record a run of `program` and find its handler.
    fn recorded(&self, program: &Path, args: &[&str]) -> (Vec<String>, Option<Handler>) {
        let path = normalize(program);
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let mut st = self.st();
        st.calls.push(Call {
            program: path.clone(),
            args: args.clone(),
        });
        let users = (!self.root_env)
            .then(|| st.users_env_programs.get(&path).cloned())
            .flatten();
        let handler = users.or_else(|| st.programs.get(&path).cloned());
        (args, handler)
    }
}
