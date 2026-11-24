//! `lmgw-doc-reader run [rules] -- <program> <args…>`
//! ([`lmgw_doc_reader::sandbox`]): another program started confined, in
//! this process's place.
//!
//! In this order, before anything else and before any document byte exists
//! here:
//! 1. every descriptor above stdin, stdout and stderr is closed (what the
//!    gateway's process held without close-on-exec);
//! 2. `RLIMIT_AS`, `RLIMIT_FSIZE` and `RLIMIT_CPU` are set as given (the
//!    CPU bound's hard limit a second above its soft one, so `SIGXCPU`
//!    comes first and `SIGKILL` follows);
//! 3. a Landlock ruleset is built that handles every filesystem right of
//!    the ABI the kernel reports (best effort upward: `refer` from ABI 2,
//!    `truncate` from 3, `ioctl_dev` from 5, `resolve_unix` from 9), TCP
//!    bind and connect from ABI 4 and UDP bind and send from ABI 10 when
//!    `--deny-net` is given, and the abstract-socket and signal scopes from
//!    ABI 6 (`sandbox::abi`, which also names what an older ABI lacks); only
//!    the rules' paths are allowed anything, each the rights of its class
//!    (a `--read-write` path: no symbolic link made and nothing linked or
//!    moved in from another directory, `abi::WRITE`). `PR_SET_NO_NEW_PRIVS`
//!    is set and the ruleset applied to this process, which every process
//!    it starts inherits;
//! 4. the program is `exec`ed.
//!
//! Any step that fails ends the run with
//! [`sandbox::EXIT_UNCONFINED`] and the program is not started: there is
//! no unconfined run. A rule path that does not exist is skipped (nothing
//! is there to allow). Landlock is Linux only; on other systems a `run`
//! with rules is refused the same way (they have no confinement yet).
//!
//! `--limits-only` ([`Confine::limits_only`]) is steps 1, 2 and 4 alone: a
//! tool lmgw does not confine yet, or a run where Landlock is unavailable.
//! It works on every unix system (descriptors walked where there is no
//! `close_range`), so the gateway starts no foreign tool with a fork.

use std::ffi::OsString;

use lmgw_doc_reader::sandbox::{self, Confine};

/// Run the subcommand with `args` (after `run`); never returns.
pub fn main(args: &[OsString]) -> ! {
    // Every line about the reader's own failure starts with the run's
    // prefix (`sandbox::status_prefix`, review N1).
    let say = sandbox::status_prefix(sandbox::token_of(args).as_deref());
    if args.len() == 1 && args[0] == sandbox::PROBE {
        match imp::probe() {
            Ok(abi) => {
                println!("{}", sandbox::probe_line(abi));
                std::process::exit(0);
            }
            Err(why) => {
                eprintln!("{say}{why}");
                std::process::exit(sandbox::EXIT_UNCONFINED);
            }
        }
    }
    let (confine, program, rest) = match Confine::parse(args) {
        Ok(p) => p,
        Err(why) => {
            eprintln!(
                "{say}{why} (usage: run [--read-exec P]… [--read P]… [--read-write P]… \
                 [--make-sock P]… [--read-stdin] [--deny-net] [--limits-only] \
                 [--address-space BYTES] [--file-size BYTES] [--cpu-seconds N] \
                 [--status-token WORD] -- \
                 /absolute/program [args…] | run --probe)"
            );
            std::process::exit(sandbox::EXIT_UNCONFINED);
        }
    };
    if let Err(why) = confine_self(&confine) {
        eprintln!("{say}{why}, so {} was not started", program.display());
        std::process::exit(sandbox::EXIT_UNCONFINED);
    }
    let e = exec(&program, &rest);
    eprintln!("{say}{} could not be started: {e}", program.display());
    std::process::exit(if e.kind() == std::io::ErrorKind::NotFound {
        sandbox::EXIT_NOT_FOUND
    } else {
        sandbox::EXIT_NOT_STARTED
    });
}

/// The reader's own parsing run confined to nothing (`imp::nothing`):
/// after its limits are set and before it reads a byte of stdin.
pub fn nothing() -> Result<(), String> {
    imp::nothing().map(drop)
}

/// This process as `c` asks: the descriptors closed and the limits set
/// (every unix system), then the Landlock ruleset unless it is
/// [`Confine::limits_only`].
fn confine_self(c: &Confine) -> Result<(), String> {
    limits::apply(c)?;
    if c.limits_only {
        return Ok(());
    }
    imp::confine(c)
}

/// `program` with `args` in this process's place; returns only when it
/// could not be started.
#[cfg(unix)]
fn exec(program: &std::path::Path, args: &[OsString]) -> std::io::Error {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = |s: &[u8]| CString::new(s).ok();
    let Some(prog) = c(program.as_os_str().as_bytes()) else {
        return std::io::Error::other("the program's path holds a NUL byte");
    };
    let mut argv: Vec<CString> = vec![prog.clone()];
    for a in args {
        match c(a.as_bytes()) {
            Some(a) => argv.push(a),
            None => return std::io::Error::other("an argument holds a NUL byte"),
        }
    }
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    // SAFETY: `prog` and `ptrs` (NULL-terminated) live across the call.
    unsafe { libc::execv(prog.as_ptr(), ptrs.as_ptr()) };
    std::io::Error::last_os_error()
}

/// No program is started this way off unix yet.
#[cfg(not(unix))]
fn exec(_program: &std::path::Path, _args: &[OsString]) -> std::io::Error {
    std::io::Error::other("programs are started through lmgw's reader on unix only yet")
}

/// Steps 1 and 2 (module doc): the inherited descriptors and the limits.
#[cfg(unix)]
mod limits {
    use lmgw_doc_reader::sandbox::Confine;

    fn errno() -> std::io::Error {
        std::io::Error::last_os_error()
    }

    /// Every descriptor from 3 up closed: `close_range` on Linux (5.9 and
    /// newer), each one up to the open-file limit elsewhere.
    fn close_inherited() -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: no descriptor from 3 up is in use by this process.
            let closed = unsafe {
                libc::syscall(
                    libc::SYS_close_range,
                    3 as libc::c_uint,
                    libc::c_uint::MAX,
                    0,
                )
            };
            if closed != 0 {
                return Err(format!(
                    "the inherited descriptors could not be closed: {}",
                    errno()
                ));
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // SAFETY: plain calls on this process's own descriptors, none
            // of which from 3 up is in use.
            let open_max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) }.clamp(3, 65_536) as i32;
            for fd in 3..open_max {
                // SAFETY: as above.
                unsafe { libc::close(fd) };
            }
        }
        Ok(())
    }

    /// Which limit [`limit`] sets.
    enum Which {
        AddressSpace,
        FileSize,
        Cpu,
    }

    fn limit(which: Which, n: u64) -> Result<(), String> {
        let hard = match which {
            // `SIGXCPU` at the bound, `SIGKILL` a second later (module doc).
            Which::Cpu => n.saturating_add(1),
            _ => n,
        };
        let lim = libc::rlimit {
            rlim_cur: n as libc::rlim_t,
            rlim_max: hard as libc::rlim_t,
        };
        // SAFETY: `lim` is valid for the call.
        let (r, what) = unsafe {
            match which {
                Which::AddressSpace => (libc::setrlimit(libc::RLIMIT_AS, &lim), "the memory limit"),
                Which::FileSize => (
                    libc::setrlimit(libc::RLIMIT_FSIZE, &lim),
                    "the file-size limit",
                ),
                Which::Cpu => (
                    libc::setrlimit(libc::RLIMIT_CPU, &lim),
                    "the CPU time bound",
                ),
            }
        };
        if r != 0 {
            return Err(format!("{what} could not be set: {}", errno()));
        }
        Ok(())
    }

    pub fn apply(c: &Confine) -> Result<(), String> {
        close_inherited()?;
        if let Some(n) = c.address_space {
            limit(Which::AddressSpace, n)?;
        }
        if let Some(n) = c.file_size {
            limit(Which::FileSize, n)?;
        }
        if let Some(n) = c.cpu_seconds {
            limit(Which::Cpu, n)?;
        }
        Ok(())
    }
}

/// No limits can be set off unix yet: every `run` is refused.
#[cfg(not(unix))]
mod limits {
    use lmgw_doc_reader::sandbox::Confine;

    pub fn apply(_c: &Confine) -> Result<(), String> {
        Err("programs are started through lmgw's reader on unix only yet".to_string())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use lmgw_doc_reader::sandbox::Confine;

    use lmgw_doc_reader::sandbox::abi::{self, FS_EXECUTE, FS_FILE, FS_MAKE_SOCK, FS_READ_FILE};

    const CREATE_RULESET_VERSION: u32 = 1 << 0;
    const RULE_PATH_BENEATH: i32 = 1;

    /// `struct landlock_ruleset_attr`, as far as lmgw fills it in.
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }

    /// `struct landlock_path_beneath_attr` (packed).
    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    fn errno() -> std::io::Error {
        std::io::Error::last_os_error()
    }

    /// The Landlock ABI the kernel offers.
    fn abi() -> Result<u32, String> {
        // SAFETY: the version query takes no attribute (NULL, 0).
        let v = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        if v < 1 {
            let e = errno();
            return Err(match e.raw_os_error() {
                Some(libc::ENOSYS) => {
                    "this kernel has no Landlock (it needs Linux 5.13 or newer)".to_string()
                }
                Some(libc::EOPNOTSUPP) => {
                    "Landlock is not enabled in this kernel (it is not in the kernel's list of \
                     security modules, lsm=)"
                        .to_string()
                }
                _ => format!("Landlock could not be queried: {e}"),
            });
        }
        Ok(v as u32)
    }

    /// A ruleset handling everything `abi` knows (the network when
    /// `deny_net`, `abi::ruleset`): its descriptor.
    fn ruleset(abi: u32, deny_net: bool) -> Result<i32, String> {
        let r = abi::ruleset(abi, deny_net);
        let attr = RulesetAttr {
            handled_access_fs: r.handled_fs,
            handled_access_net: r.handled_net,
            scoped: r.scoped,
        };
        let size = r.size.min(std::mem::size_of::<RulesetAttr>());
        // SAFETY: `attr` is valid for `size` bytes for the call.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                size,
                0u32,
            )
        };
        if fd < 0 {
            return Err(format!("no Landlock ruleset could be made: {}", errno()));
        }
        Ok(fd as i32)
    }

    /// Allow `rights` beneath `path` in `ruleset` (a file: its file rights).
    fn allow(ruleset: i32, path: &Path, rights: u64) -> Result<(), String> {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("{} holds a NUL byte", path.display()))?;
        // SAFETY: `c` is a valid C string; the descriptor is closed below.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            let e = errno();
            if e.kind() == std::io::ErrorKind::NotFound {
                return Ok(());
            }
            return Err(format!(
                "{} could not be opened for a rule: {e}",
                path.display()
            ));
        }
        // SAFETY: an all-zero `stat` is a valid value to be filled in.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is open; `st` is valid for writing.
        let is_dir = unsafe { libc::fstat(fd, &mut st) } == 0
            && (st.st_mode & libc::S_IFMT) == libc::S_IFDIR;
        let allowed = if is_dir { rights } else { rights & FS_FILE };
        let attr = PathBeneath {
            allowed_access: allowed,
            parent_fd: fd,
        };
        let added = if allowed == 0 {
            0
        } else {
            // SAFETY: `attr` is valid for the call; `ruleset` is a ruleset.
            unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    ruleset,
                    RULE_PATH_BENEATH,
                    &attr as *const PathBeneath,
                    0u32,
                )
            }
        };
        let e = errno();
        // SAFETY: `fd` is ours.
        unsafe { libc::close(fd) };
        if added != 0 {
            return Err(format!("the rule for {} was refused: {e}", path.display()));
        }
        Ok(())
    }

    /// Allow `rights` on the file that is stdin, which must be a regular
    /// file (`Confine::read_stdin`): a rule on its inode alone, so nothing
    /// beside it in its directory is reachable.
    fn allow_stdin(ruleset: i32, rights: u64) -> Result<(), String> {
        // SAFETY: an all-zero `stat` is a valid value to be filled in.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: descriptor 0 is the process's; `st` is valid for writing.
        if unsafe { libc::fstat(0, &mut st) } != 0 {
            return Err(format!("stdin could not be read for its rule: {}", errno()));
        }
        if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
            return Err("--read-stdin needs stdin to be a regular file".to_string());
        }
        let attr = PathBeneath {
            allowed_access: rights,
            parent_fd: 0,
        };
        // SAFETY: `attr` is valid for the call; `ruleset` is a ruleset.
        let added = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset,
                RULE_PATH_BENEATH,
                &attr as *const PathBeneath,
                0u32,
            )
        };
        if added != 0 {
            return Err(format!("the rule for stdin was refused: {}", errno()));
        }
        Ok(())
    }

    fn restrict(ruleset: i32) -> Result<(), String> {
        // SAFETY: plain calls on this process.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(format!("no_new_privs could not be set: {}", errno()));
        }
        // SAFETY: `ruleset` is a ruleset descriptor.
        let r = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) };
        let e = errno();
        // SAFETY: the ruleset is ours.
        unsafe { libc::close(ruleset) };
        if r != 0 {
            return Err(format!("the Landlock ruleset could not be applied: {e}"));
        }
        Ok(())
    }

    /// A ruleset that allows nothing (no file opened, no network), applied
    /// to this process where Landlock is there: the reader's own parsing
    /// runs, which open nothing once they have started. `Ok(None)` where
    /// the kernel has no Landlock (they run as before); a ruleset that
    /// cannot be applied once Landlock is there is `Err`.
    pub fn nothing() -> Result<Option<u32>, String> {
        let Ok(abi) = abi() else {
            return Ok(None);
        };
        restrict(ruleset(abi, true)?)?;
        Ok(Some(abi))
    }

    /// A ruleset that allows nothing, applied to this process: whether a
    /// confinement can be applied here, and the ABI it had.
    pub fn probe() -> Result<u32, String> {
        let abi = abi()?;
        restrict(ruleset(abi, true)?)?;
        Ok(abi)
    }

    /// This process confined by the Landlock ruleset `c` names (module
    /// doc, step 3; [`super::limits`] did steps 1 and 2).
    pub fn confine(c: &Confine) -> Result<(), String> {
        let abi = abi()?;
        let read = abi::READ;
        let write = abi::WRITE;
        let set = ruleset(abi, c.deny_net)?;
        let classes = [
            (&c.read_exec, read | FS_EXECUTE),
            (&c.read, read),
            (&c.read_write, read | write),
            (&c.make_sock, FS_MAKE_SOCK),
        ];
        for (paths, rights) in classes {
            for p in paths {
                if let Err(e) = allow(set, p, abi::allowed(abi, rights)) {
                    // SAFETY: the ruleset is ours.
                    unsafe { libc::close(set) };
                    return Err(e);
                }
            }
        }
        if c.read_stdin {
            if let Err(e) = allow_stdin(set, abi::allowed(abi, FS_READ_FILE)) {
                // SAFETY: the ruleset is ours.
                unsafe { libc::close(set) };
                return Err(e);
            }
        }
        restrict(set)
    }
}

/// No confinement on this system yet: every `run` with rules is refused.
#[cfg(not(target_os = "linux"))]
mod imp {
    use lmgw_doc_reader::sandbox::Confine;

    const NOT_YET: &str = "programs are confined on Linux only yet (Landlock)";

    pub fn probe() -> Result<u32, String> {
        Err(NOT_YET.to_string())
    }

    pub fn confine(_c: &Confine) -> Result<(), String> {
        Err(NOT_YET.to_string())
    }

    /// No Landlock here: the parsing runs go on as before.
    pub fn nothing() -> Result<Option<u32>, String> {
        Ok(None)
    }
}
