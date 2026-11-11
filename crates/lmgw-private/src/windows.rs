//! The Windows side (crate doc): the account's SID, the check, the creates.

mod check;
mod create;
mod sid;

pub use check::{check_handle, check_path, Found, NotPrivate};
pub use create::{
    create_dir, create_dir_all, create_new_file, make_private, open_no_follow, OpenError,
};
pub use sid::{account_name, Sid};

/// `path` as the NUL-terminated UTF-16 the wide API takes.
fn wide(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
