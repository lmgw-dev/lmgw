//! [`super::SystemFacts`] on Windows (Windows design §4.4, WP6): `stat` and
//! `lstat` from the file's handle (kind, size, NTFS file ID, write and change
//! times; no POSIX mode or owner, so [`Meta::posix`] is `None`), free space
//! from `GetDiskFreeSpaceExW` with the volume's serial number as its ID, the
//! user's name from `USERNAME`. The Linux readers (CDI specs, rpm, dnf) are
//! not run on Windows; the Windows machine check reads what it needs here.

use std::fs::File;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::process::{Child, Command};

use windows_sys::Win32::Storage::FileSystem::{
    FileBasicInfo, GetDiskFreeSpaceExW, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetVolumeInformationByHandleW, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE,
};

use super::{Kind, Meta, Space};

/// 100-nanosecond intervals between 1601-01-01 (FILETIME's epoch) and the
/// Unix epoch.
const FILETIME_UNIX_EPOCH: i128 = 116_444_736_000_000_000;

/// A FILETIME count as nanoseconds since the Unix epoch.
fn unix_ns(filetime: i64) -> i128 {
    (i128::from(filetime) - FILETIME_UNIX_EPOCH) * 100
}

/// `path` opened for its attributes only (a directory too), the link itself
/// when `no_follow`.
fn open_attributes(path: &Path, no_follow: bool) -> io::Result<File> {
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if no_follow {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(flags)
        .open(path)
}

fn meta_of(file: &File) -> io::Result<Meta> {
    let h = file.as_raw_handle();
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: the handle is open for the calls; each struct is the one its
    // call names, at its own size.
    unsafe {
        if GetFileInformationByHandle(h, &mut info) == 0
            || GetFileInformationByHandleEx(
                h,
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let attrs = info.dwFileAttributes;
    let kind = if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        // A symbolic link or a junction, opened as itself.
        Kind::Symlink
    } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Kind::Dir
    } else {
        Kind::File
    };
    Ok(Meta {
        kind,
        posix: None,
        len: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        ino: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        mtime_ns: unix_ns(basic.LastWriteTime),
        ctime_ns: unix_ns(basic.ChangeTime),
    })
}

pub(super) fn stat(path: &Path) -> io::Result<Meta> {
    meta_of(&open_attributes(path, false)?)
}

pub(super) fn lstat(path: &Path) -> io::Result<Meta> {
    meta_of(&open_attributes(path, true)?)
}

/// Windows has no numeric user IDs.
pub(super) fn uid() -> Option<u32> {
    None
}

/// The signed-in user's name, as Windows sets it for every process.
pub(super) fn user_name() -> Option<String> {
    std::env::var("USERNAME").ok().filter(|n| !n.is_empty())
}

pub(super) fn spawn_killable(cmd: &mut Command) -> io::Result<Child> {
    cmd.spawn()
}

/// Only the program itself is stopped: what it started keeps running, as
/// Windows has no process groups to signal (a job object would reach them;
/// not yet: the Windows machine check runs no program that starts others).
pub(super) fn kill_all(mut child: Child) {
    let _ = child.kill();
    std::thread::spawn(move || child.wait());
}

/// The room on the volume `path` is on: what this user may still write
/// (quotas count), the volume's size, and its serial number as the ID.
pub(super) fn space(path: &Path) -> io::Result<Space> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let (mut free, mut total, mut all_free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated; the three outputs are u64s of ours.
    if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut all_free) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let file = open_attributes(path, false)?;
    let mut serial = 0u32;
    // SAFETY: the handle is open; only the serial number is asked for.
    if unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Space {
        free_bytes: free,
        total_bytes: total,
        fs_id: u64::from(serial),
    })
}
