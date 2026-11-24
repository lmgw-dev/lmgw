//! A file a tool wrote, opened without following a link (security round
//! B, L3): the gateway is not confined, so a tool that left a symbolic link
//! where its output should be (or put one in place of a directory on the
//! way) would make it read whatever the link names. Each step from the run
//! directory down is opened with `O_NOFOLLOW` relative to the one before
//! (`openat`), so a link anywhere on the way is refused, and the end must
//! be a regular file.

use std::fs::File;
use std::path::Path;

/// `rel` (plain names, no `..`) beneath `dir`, opened to read.
#[cfg(unix)]
pub fn open(dir: &Path, rel: &Path) -> std::io::Result<File> {
    use std::ffi::CString;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;
    let c =
        |s: &[u8]| CString::new(s).map_err(|_| std::io::Error::other("a path holds a NUL byte"));
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let d = c(dir.as_os_str().as_bytes())?;
    // SAFETY: `d` is a valid C string; the descriptor is owned below.
    let fd = unsafe { libc::open(d.as_ptr(), flags | libc::O_DIRECTORY) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and is owned by nothing else.
    let mut at = unsafe { OwnedFd::from_raw_fd(fd) };
    let names: Vec<&std::ffi::OsStr> = rel
        .components()
        .map(|p| match p {
            Component::Normal(n) => Ok(n),
            _ => Err(std::io::Error::other(
                "only plain names are opened beneath a directory",
            )),
        })
        .collect::<Result<_, _>>()?;
    let Some((last, ways)) = names.split_last() else {
        return Err(std::io::Error::other("no file named"));
    };
    for name in ways {
        at = open_at(&at, &c(name.as_bytes())?, flags | libc::O_DIRECTORY)?;
    }
    // Non-blocking: a FIFO in its place must not hang the open (it is
    // refused below as no regular file).
    let file = File::from(open_at(
        &at,
        &c(last.as_bytes())?,
        flags | libc::O_NONBLOCK,
    )?);
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("it is not a regular file"));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_at(
    at: &std::os::fd::OwnedFd,
    name: &std::ffi::CStr,
    flags: libc::c_int,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // SAFETY: `at` is an open directory and `name` a valid C string.
    let fd = unsafe { libc::openat(at.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: just opened, owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Off unix no tool writes into a run directory yet: a plain open, a
/// regular file required.
#[cfg(not(unix))]
pub fn open(dir: &Path, rel: &Path) -> std::io::Result<File> {
    let file = File::open(dir.join(rel))?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("it is not a regular file"));
    }
    Ok(file)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn a_link_on_the_way_or_at_the_end_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let secret = tempfile::tempdir().unwrap();
        std::fs::write(secret.path().join("in.docx"), "not the tool's").unwrap();
        std::fs::create_dir(t.path().join("out")).unwrap();
        std::fs::write(t.path().join("out/in.docx"), "the tool's").unwrap();
        let mut s = String::new();
        open(t.path(), Path::new("out/in.docx"))
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "the tool's");
        // The file a link.
        std::fs::remove_file(t.path().join("out/in.docx")).unwrap();
        std::os::unix::fs::symlink(secret.path().join("in.docx"), t.path().join("out/in.docx"))
            .unwrap();
        assert!(open(t.path(), Path::new("out/in.docx")).is_err());
        // A directory on the way a link.
        std::fs::remove_dir_all(t.path().join("out")).unwrap();
        std::os::unix::fs::symlink(secret.path(), t.path().join("out")).unwrap();
        assert!(open(t.path(), Path::new("out/in.docx")).is_err());
        // Not a regular file, `..`, nothing named.
        std::fs::create_dir(t.path().join("d")).unwrap();
        assert!(open(t.path(), Path::new("d")).is_err());
        assert!(open(t.path(), Path::new("../x")).is_err());
        assert!(open(t.path(), Path::new("")).is_err());
    }
}
