//! Creates and opens (crate doc): a private security descriptor from the
//! start, and no link followed.

use std::fs::File;
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{
    LocalFree, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, ACL, DACL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, CREATE_NEW, DELETE, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    WRITE_DAC, WRITE_OWNER,
};

use super::sid::Sid;
use super::wide;

/// Why an open without following links failed.
#[derive(Debug)]
pub enum OpenError {
    /// A symbolic link or junction stands at the path.
    Link,
    Io(io::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Link => f.write_str("it is a symbolic link or junction"),
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for OpenError {}

/// A security descriptor the SDDL conversion allocated; freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptor…, freed once.
        unsafe { LocalFree(self.0) };
    }
}

/// The private descriptor (crate doc): owner the account lmgw runs as, a
/// protected DACL of full access for it, `SYSTEM` and the Administrators.
/// A directory's entries are inherited by what is made in it.
fn private_descriptor(dir: bool) -> io::Result<Descriptor> {
    let me = Sid::current_user()?.string();
    let inherit = if dir { "OICI" } else { "" };
    let sddl =
        format!("O:{me}D:P(A;{inherit};FA;;;{me})(A;{inherit};FA;;;SY)(A;{inherit};FA;;;BA)");
    let text: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `text` is NUL-terminated; the descriptor is allocated by the
    // call and owned by the Descriptor returned.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Descriptor(sd))
}

fn attributes(sd: &Descriptor) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    }
}

/// Create the directory `path` (its parent exists) with the private
/// descriptor. One that is already there is an error of kind
/// [`io::ErrorKind::AlreadyExists`]: the caller checks it.
pub fn create_dir(path: &Path) -> io::Result<()> {
    let sd = private_descriptor(true)?;
    let sa = attributes(&sd);
    // SAFETY: the path is NUL-terminated; `sa` and its descriptor outlive
    // the call.
    if unsafe { CreateDirectoryW(wide(path).as_ptr(), &sa) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// [`create_dir`] for `path` and every missing ancestor; what is there
/// already is left as it is.
pub fn create_dir_all(path: &Path) -> io::Result<()> {
    if path.symlink_metadata().is_ok() {
        return Ok(());
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_dir_all(parent)?;
    }
    match create_dir(path) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

/// Create `path` as a new file, open to read and write, with the private
/// descriptor (crate doc): `CREATE_NEW`, so a file or a link already there is
/// an error of kind [`io::ErrorKind::AlreadyExists`] and is never followed or
/// truncated. No other open shares it while it is open.
pub fn create_new_file(path: &Path) -> io::Result<File> {
    let sd = private_descriptor(false)?;
    let sa = attributes(&sd);
    // SAFETY: the path is NUL-terminated; `sa` and its descriptor outlive
    // the call; the handle is owned by the File returned.
    let handle = unsafe {
        CreateFileW(
            wide(path).as_ptr(),
            GENERIC_READ | GENERIC_WRITE | DELETE,
            0,
            &sa,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle of ours.
    Ok(File::from(unsafe { OwnedHandle::from_raw_handle(handle) }))
}

/// Open `path` to read (and to `write`, if asked) without following a link:
/// a symbolic link or junction there is [`OpenError::Link`]. The handle can
/// read its own security descriptor ([`super::check_handle`]).
pub fn open_no_follow(path: &Path, write: bool) -> Result<File, OpenError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(OpenError::Io)?;
    refuse_link(file)
}

/// `path` opened for its metadata and security descriptor only, a directory
/// too, without following a link.
pub(super) fn open_meta(path: &Path) -> Result<File, OpenError> {
    let file = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .map_err(OpenError::Io)?;
    refuse_link(file)
}

fn refuse_link(file: File) -> Result<File, OpenError> {
    let meta = file.metadata().map_err(OpenError::Io)?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(OpenError::Link);
    }
    Ok(file)
}

/// Give what is at `path` (a file or a directory, not a link) the private
/// descriptor (crate doc): its owner set to the account lmgw runs as and its
/// DACL replaced by a protected one, as `chmod 700` closes a directory on
/// Linux.
pub fn make_private(path: &Path) -> Result<(), OpenError> {
    let file = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .map_err(OpenError::Io)?;
    let file = refuse_link(file)?;
    let is_dir = file.metadata().map_err(OpenError::Io)?.is_dir();
    let sd = private_descriptor(is_dir).map_err(OpenError::Io)?;
    let (mut present, mut defaulted) = (0, 0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut owner: PSID = std::ptr::null_mut();
    // SAFETY: `sd` is a valid descriptor; the pointers it answers point
    // into it, which outlives the SetSecurityInfo below.
    unsafe {
        if GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) == 0
            || GetSecurityDescriptorOwner(sd.0, &mut owner, &mut defaulted) == 0
        {
            return Err(OpenError::Io(io::Error::last_os_error()));
        }
        let rc = SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            owner,
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        );
        if rc != 0 {
            return Err(OpenError::Io(io::Error::from_raw_os_error(rc as i32)));
        }
    }
    Ok(())
}
