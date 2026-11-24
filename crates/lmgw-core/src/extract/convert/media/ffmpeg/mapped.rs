//! A finished WAV's bytes, mapped (`ffmpeg.rs`'s module doc): the pages
//! are the page cache's, read on demand and dropped by the kernel under
//! pressure, never the gateway's own memory.
//!
//! What is mapped is a copy only the gateway can reach (the review of
//! security round B, S3), never ffmpeg's own file: a process of the run
//! that left its process group (`setsid`) survives the group kill, and one
//! holding a descriptor of the WAV it opened while it could write it could
//! cut the file under a mapping, which ends the whole gateway with
//! `SIGBUS` at the next page it reads. The copy is an unnamed file in the
//! spool's root (`O_TMPFILE` where the filesystem has it, else created and
//! unlinked at once): no tool's rules reach that directory, only their own
//! run directories below it, and a file without a name cannot be opened by
//! one. It is a clone of the WAV's blocks where the filesystem can share
//! them (`FICLONE`: btrfs, XFS), else a copy in the kernel
//! (`copy_file_range`), at most `limit` bytes. ffmpeg's file is opened
//! without following a link (`crate::child::beneath`; L3).

use std::path::Path;

use bytes::Bytes;

/// The file `name` in the run's directory `dir`, copied into an unnamed
/// file under `under` (at most `limit` bytes) and mapped (an empty file:
/// no bytes).
#[cfg(unix)]
pub fn map(dir: &Path, name: &str, under: &Path, limit: u64) -> std::io::Result<Bytes> {
    use std::os::fd::AsRawFd;
    let source = crate::child::beneath::open(dir, Path::new(name))?;
    let file = own_copy(&source, under, limit)?;
    drop(source);
    let len = usize::try_from(file.metadata()?.len())
        .map_err(|_| std::io::Error::other("the file is larger than this system can map"))?;
    if len == 0 {
        return Ok(Bytes::new());
    }
    // SAFETY: a read-only private mapping of an open file of `len` bytes
    // that nothing but this process can reach (module doc); the result is
    // checked before use.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Bytes::from_owner(Mapped { ptr, len }))
}

/// `source` as an unnamed file under `under` that only this process holds
/// (module doc): its blocks shared, else copied, at most `limit` bytes.
#[cfg(unix)]
fn own_copy(source: &std::fs::File, under: &Path, limit: u64) -> std::io::Result<std::fs::File> {
    let mut copy = tempfile::tempfile_in(under)?;
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // `FICLONE`, `_IOW(0x94, 9, int)`: the whole file's blocks shared.
        const FICLONE: libc::c_ulong = 0x4004_9409;
        let small = source.metadata()?.len() <= limit;
        // SAFETY: both descriptors are open; the call reads no memory.
        if small && unsafe { libc::ioctl(copy.as_raw_fd(), FICLONE as _, source.as_raw_fd()) } == 0
        {
            return Ok(copy);
        }
    }
    // `std::io::copy` between files is `copy_file_range` on Linux.
    let mut from = std::io::Read::take(source, limit);
    std::io::copy(&mut from, &mut copy)?;
    Ok(copy)
}

/// Elsewhere the file is read (no tool writes one there yet: ffmpeg is not
/// run on Windows yet).
#[cfg(not(unix))]
pub fn map(dir: &Path, name: &str, _under: &Path, limit: u64) -> std::io::Result<Bytes> {
    use std::io::Read;
    let mut out = Vec::new();
    crate::child::beneath::open(dir, Path::new(name))?
        .take(limit)
        .read_to_end(&mut out)?;
    Ok(Bytes::from(out))
}

/// A read-only mapping, unmapped when dropped.
#[cfg(unix)]
struct Mapped {
    ptr: *mut libc::c_void,
    len: usize,
}

// SAFETY: the mapping is read-only and owned by this value alone; reading
// it from any thread is sound.
#[cfg(unix)]
unsafe impl Send for Mapped {}
// SAFETY: as above: shared access is reads of immutable memory.
#[cfg(unix)]
unsafe impl Sync for Mapped {}

#[cfg(unix)]
impl AsRef<[u8]> for Mapped {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: `ptr` maps `len` readable bytes for this value's life.
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }
}

#[cfg(unix)]
impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `map`, unmapped once.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_mapped_file_outlives_its_name() {
        let root = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir_in(root.path()).unwrap();
        let p = dir.path().join("out.wav");
        std::fs::write(&p, b"RIFF....WAVE").unwrap();
        let b = map(dir.path(), "out.wav", root.path(), 1 << 20).unwrap();
        drop(dir);
        assert_eq!(&b[..], b"RIFF....WAVE");
        assert!(map(Path::new("/nonexistent.example"), "x", root.path(), 1).is_err());
        // A link in its place is refused (security round B, L3).
        let dir = tempfile::tempdir_in(root.path()).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.path().join("out.wav")).unwrap();
        assert!(map(dir.path(), "out.wav", root.path(), 1 << 20).is_err());
    }

    /// The review of round B, S3: the original cut to nothing under the
    /// mapping (a process of the run that still holds it) leaves the
    /// mapped bytes whole, and no file was left in the root. Past `limit`
    /// only `limit` bytes are copied.
    #[test]
    fn the_mapping_is_of_a_copy_a_tool_cannot_cut() {
        let root = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir_in(root.path()).unwrap();
        let p = dir.path().join("out.wav");
        let body: Vec<u8> = (0..(1u32 << 20)).map(|i| i as u8).collect();
        std::fs::write(&p, &body).unwrap();
        let held = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        let b = map(dir.path(), "out.wav", root.path(), 4 << 20).unwrap();
        held.set_len(0).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 0);
        assert_eq!(&b[..], &body[..]);
        let left: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, vec![dir.path().file_name().unwrap().to_os_string()]);
        let cut = map(dir.path(), "out.wav", root.path(), 4 << 20).unwrap();
        assert!(cut.is_empty());
        std::fs::write(&p, &body).unwrap();
        assert_eq!(
            map(dir.path(), "out.wav", root.path(), 1000).unwrap().len(),
            1000
        );
    }
}
