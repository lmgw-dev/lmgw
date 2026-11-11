//! Private files and directories on Windows (Windows design §4.4, WP6).
//!
//! On Linux lmgw keeps a secret in a `0700` directory as a `0600` file,
//! checked by uid and mode, opened with `O_NOFOLLOW`. This crate is the
//! Windows counterpart, and what "private" means there:
//!
//! - **The owner** is the account lmgw runs as (its token's user SID),
//!   `SYSTEM` or the built-in Administrators: the two may hold every right
//!   anyway (below), and a file made in an elevated session (a main TLS key
//!   written by an administrator's tool) is the Administrators' by default.
//!   A file or directory lmgw makes names the account lmgw runs as in its
//!   security descriptor, so an elevated start makes the same owner.
//! - **The access**: every allow entry of the DACL grants rights to that
//!   account, to `SYSTEM`, to the built-in Administrators, or to `CREATOR
//!   OWNER` / `OWNER RIGHTS` (which stand for the owner). Those three are what
//!   a user profile's own directories grant (`%LOCALAPPDATA%`, `%TEMP%`), and
//!   they are root's counterpart on Linux, where a `0600` file is root's to
//!   read as well. Any other account or group with a right is a finding;
//!   a missing (NULL) DACL, which grants everyone everything, is one too, and
//!   so is an entry of a kind this crate cannot read: an object or compound
//!   allow entry, or a kind it does not know. Deny entries only take rights
//!   away, and the kinds that belong in a SACL (audit, alarm, label, …) grant
//!   none; both are let be. An inherit-only entry, which applies to what is
//!   made inside a directory rather than to the directory, is judged as if
//!   it applied to it as well: conservative, a finding rather than a miss.
//! - **What a right is** depends on the check ([`Others`]): a key file allows
//!   others nothing that reads or changes it ([`Others::Nothing`]); a
//!   directory whose files are what is private allows nothing that changes
//!   it ([`Others::ReadOnly`]), as Linux's `mode & 0o022` check does.
//! - **Made private**: [`create_dir`] and [`create_new_file`] give a new
//!   directory or file a *protected* DACL (nothing inherited from the parent)
//!   of exactly those three entries, in the same call that creates it, so
//!   no other account ever had a moment of access (the `mode` of a Linux
//!   `mkdir`/`open`). [`make_private`] replaces an existing one's (Linux's
//!   `chmod 700`).
//! - **No link is followed**: every open here passes
//!   `FILE_FLAG_OPEN_REPARSE_POINT`, so a symbolic link or junction is opened
//!   as itself and refused ([`OpenError::Link`]); a create is `CREATE_NEW`,
//!   which a link in the way fails as "exists".
//!
//! On Unix this crate has nothing: each caller keeps its own mode and uid
//! code there.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// What accounts other than the owner (and `SYSTEM` and the Administrators)
/// may hold on a checked object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Others {
    /// No right that reads or changes it: a key file.
    Nothing,
    /// No right that changes it (they may read or list it): a directory whose
    /// files are what is private, as Linux's `mode & 0o022` check.
    ReadOnly,
}
