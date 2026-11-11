//! [`super::SystemFacts`]' POSIX reads: `stat` with mode, owner and inode,
//! the user database, process groups and `statvfs`.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};

use super::{Kind, Meta, Posix, Space};

pub(super) fn stat(path: &Path) -> io::Result<Meta> {
    std::fs::metadata(path).map(|m| meta_of(&m))
}

pub(super) fn lstat(path: &Path) -> io::Result<Meta> {
    std::fs::symlink_metadata(path).map(|m| meta_of(&m))
}

fn meta_of(m: &std::fs::Metadata) -> Meta {
    let ft = m.file_type();
    let kind = if ft.is_symlink() {
        Kind::Symlink
    } else if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    Meta {
        kind,
        posix: Some(Posix {
            mode: m.mode() & 0o7777,
            uid: m.uid(),
        }),
        len: m.len(),
        ino: m.ino(),
        mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
        ctime_ns: i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec()),
    }
}

pub(super) fn uid() -> Option<u32> {
    // SAFETY: getuid has no preconditions and cannot fail.
    Some(unsafe { libc::getuid() })
}

pub(super) fn user_name() -> Option<String> {
    let uid = uid()?;
    let mut buf = vec![0u8; 1024];
    loop {
        // SAFETY: an all-zero passwd is a valid value of the plain C
        // struct; getpwuid_r writes only into it and into `buf`, whose
        // length it is given, and `result` points at `pwd` or is null.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut pwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        // A buffer too small for the entry: a bigger one, until it fits.
        if rc == libc::ERANGE {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() || pwd.pw_name.is_null() {
            return None;
        }
        // SAFETY: on success pw_name points at a NUL-terminated string
        // inside `buf`, which outlives this read.
        let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
        return Some(name.to_string_lossy().into_owned());
    }
}

/// `cmd` started in its own process group, so [`kill_all`] reaches
/// whatever it started too.
pub(super) fn spawn_killable(cmd: &mut Command) -> io::Result<Child> {
    cmd.process_group(0).spawn()
}

/// SIGKILL to `child`'s process group, reaped on a thread of its own.
/// `try_wait` has just said it has not ended, so it is not reaped and its
/// pid is still its own. Not reaped here: a program stuck in the kernel (an
/// NVML call to a card that fell off the bus) ends only when that call
/// returns, and the caller must not wait for it past its limit.
pub(super) fn kill_all(mut child: Child) {
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: `kill` takes two integers and touches no memory of ours;
        // the group is the one `process_group(0)` made for this child.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    std::thread::spawn(move || child.wait());
}

/// `statvfs(2)` of `path`.
pub(super) fn space(path: &Path) -> io::Result<Space> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: an all-zero statvfs is a valid value of the plain C struct,
    // `c` is a NUL-terminated path that outlives the call, and statvfs
    // writes only into the struct it is given.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let frsize = st.f_frsize as u64;
    Ok(Space {
        free_bytes: (st.f_bavail as u64).saturating_mul(frsize),
        total_bytes: (st.f_blocks as u64).saturating_mul(frsize),
        fs_id: st.f_fsid as u64,
    })
}
