//! Model files whose size is all a test needs: gigabytes of metadata, no
//! disk. A file extended with `set_len` is sparse on Linux's filesystems by
//! itself; on NTFS it is not (the clusters are allocated, so a suite that
//! lays out tens of GiB fills the volume), unless the file is marked sparse
//! first with `FSCTL_SET_SPARSE`, which [`set_len`] does.

use std::fs::File;
use std::path::Path;

/// A new file at `path` (truncated when it exists), `len` bytes long and
/// sparse.
pub fn create(path: impl AsRef<Path>, len: u64) -> File {
    let path = path.as_ref();
    let file = File::create(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    set_len(&file, len);
    file
}

/// `file` (open for writing) at `len` bytes, the new bytes sparse.
pub fn set_len(file: &File, len: u64) {
    #[cfg(windows)]
    mark_sparse(file);
    file.set_len(len).unwrap();
}

#[cfg(windows)]
fn mark_sparse(file: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    let mut returned = 0u32;
    // SAFETY: the handle is open for the call; FSCTL_SET_SPARSE with no
    // input buffer sets the flag, and nothing is written to an output.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    assert!(
        ok != 0,
        "FSCTL_SET_SPARSE: {}",
        std::io::Error::last_os_error()
    );
}
