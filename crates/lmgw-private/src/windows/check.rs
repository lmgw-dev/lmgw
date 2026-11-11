//! The check (crate doc): the owner, and who else the DACL lets in.

use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{
    LocalFree, GENERIC_ALL, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    AclSizeInformation, GetAce, GetAclInformation, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
    ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_EXECUTE, FILE_READ_DATA,
    FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE, ACCESS_DENIED_ACE_TYPE,
    ACCESS_DENIED_CALLBACK_ACE_TYPE, ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE,
    ACCESS_DENIED_OBJECT_ACE_TYPE, MAXIMUM_ALLOWED, SYSTEM_ACCESS_FILTER_ACE_TYPE,
    SYSTEM_ALARM_ACE_TYPE, SYSTEM_ALARM_CALLBACK_ACE_TYPE, SYSTEM_ALARM_CALLBACK_OBJECT_ACE_TYPE,
    SYSTEM_ALARM_OBJECT_ACE_TYPE, SYSTEM_AUDIT_ACE_TYPE, SYSTEM_AUDIT_CALLBACK_ACE_TYPE,
    SYSTEM_AUDIT_CALLBACK_OBJECT_ACE_TYPE, SYSTEM_AUDIT_OBJECT_ACE_TYPE,
    SYSTEM_MANDATORY_LABEL_ACE_TYPE, SYSTEM_PROCESS_TRUST_LABEL_ACE_TYPE,
    SYSTEM_RESOURCE_ATTRIBUTE_ACE_TYPE, SYSTEM_SCOPED_POLICY_ID_ACE_TYPE,
};

use super::sid::{account_name, Sid};
use crate::Others;

/// The SIDs that may hold rights besides the owner (crate doc): `SYSTEM`,
/// the built-in Administrators, `CREATOR OWNER` and `OWNER RIGHTS`.
const TRUSTED: [&str; 4] = ["S-1-5-18", "S-1-5-32-544", "S-1-3-0", "S-1-3-4"];

/// The accounts that may own a checked object besides the account lmgw runs
/// as (crate doc): `SYSTEM` and the built-in Administrators, the two real
/// accounts of [`TRUSTED`] (an elevated start's default owner is the
/// Administrators group).
const OWNERS: [&str; 2] = ["S-1-5-18", "S-1-5-32-544"];

/// Whether the SID `owner` may own a checked object for `me`: the account
/// is compared by its bytes, the constants by their `S-1-…` form (a SID whose
/// form cannot be read matches none of them).
fn may_own(owner: &Sid, me: &Sid) -> bool {
    owner == me || OWNERS.contains(&owner.string().as_str())
}

/// What an entry of the DACL is to the check (crate doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// An allow entry laid out as `ACCESS_ALLOWED_ACE`: read.
    Allow,
    /// A deny entry, or one of the kinds that belong in a SACL (audit,
    /// alarm, label, resource attribute, scoped policy, trust label, access
    /// filter): it grants nothing.
    GrantsNothing,
    /// Any other kind (the object and compound allow entries, and every
    /// kind not known here): who it lets in is not known.
    Unknown,
}

fn entry(kind: u32) -> Entry {
    match kind {
        ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => Entry::Allow,
        ACCESS_DENIED_ACE_TYPE
        | ACCESS_DENIED_OBJECT_ACE_TYPE
        | ACCESS_DENIED_CALLBACK_ACE_TYPE
        | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE
        | SYSTEM_AUDIT_ACE_TYPE
        | SYSTEM_ALARM_ACE_TYPE
        | SYSTEM_AUDIT_OBJECT_ACE_TYPE
        | SYSTEM_ALARM_OBJECT_ACE_TYPE
        | SYSTEM_AUDIT_CALLBACK_ACE_TYPE
        | SYSTEM_ALARM_CALLBACK_ACE_TYPE
        | SYSTEM_AUDIT_CALLBACK_OBJECT_ACE_TYPE
        | SYSTEM_ALARM_CALLBACK_OBJECT_ACE_TYPE
        | SYSTEM_MANDATORY_LABEL_ACE_TYPE
        | SYSTEM_RESOURCE_ATTRIBUTE_ACE_TYPE
        | SYSTEM_SCOPED_POLICY_ID_ACE_TYPE
        | SYSTEM_PROCESS_TRUST_LABEL_ACE_TYPE
        | SYSTEM_ACCESS_FILTER_ACE_TYPE => Entry::GrantsNothing,
        _ => Entry::Unknown,
    }
}

/// Rights that change an object or what is in it.
const WRITES: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_DELETE_CHILD
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL
    | MAXIMUM_ALLOWED;

/// Rights that read an object's content (a file's data, a directory's
/// names), or run it.
const READS: u32 = FILE_READ_DATA | FILE_EXECUTE | GENERIC_READ | GENERIC_EXECUTE;

/// Why an object is not private, or could not be checked. Its `Display` is a
/// clause about "it" ("it belongs to …"); the caller names the object.
#[derive(Debug)]
pub enum NotPrivate {
    /// The check itself failed: the object could not be opened or its
    /// security descriptor read.
    Unreadable(io::Error),
    /// A symbolic link or junction stands at the path.
    Link,
    /// Its owner is another account than lmgw's, `SYSTEM` or the
    /// Administrators.
    Owner { owner: String, me: String },
    /// It has no DACL at all, which grants every account every right.
    NoDacl,
    /// Other accounts hold rights the check refuses.
    Others { who: Vec<String>, change: bool },
    /// An entry of a kind this check does not read: an object or compound
    /// allow entry, or a kind it does not know.
    UnknownEntry(u8),
}

impl std::fmt::Display for NotPrivate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(e) => write!(f, "who may read it could not be checked ({e})"),
            Self::Link => f.write_str("it is a symbolic link or junction, not a file of its own"),
            Self::Owner { owner, me } => {
                write!(
                    f,
                    "it belongs to {owner}, not to the account lmgw runs as ({me}), SYSTEM or \
                     the Administrators"
                )
            }
            Self::NoDacl => f.write_str(
                "it has no access list (a NULL DACL), which lets every account read and change it",
            ),
            Self::Others { who, change } => write!(
                f,
                "its access list lets {} {} it",
                who.join(", "),
                if *change { "change" } else { "read" }
            ),
            Self::UnknownEntry(kind) => write!(
                f,
                "its access list has an entry of a kind lmgw does not read (type {kind}), so \
                 who it lets in is not known"
            ),
        }
    }
}

impl std::error::Error for NotPrivate {}

/// What [`check_path`] found at a path that passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub is_dir: bool,
}

/// Check the object `handle` is open on (opened with `READ_CONTROL`):
/// its owner is the account lmgw runs as, and no other account holds a right
/// `others` refuses (crate doc).
pub fn check_handle(handle: BorrowedHandle<'_>, others: Others) -> Result<(), NotPrivate> {
    let me = Sid::current_user().map_err(NotPrivate::Unreadable)?;
    let mut owner: PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the handle is open for the call; the descriptor is allocated
    // by it, `owner` and `dacl` point into it, and it is freed below.
    let rc = unsafe {
        GetSecurityInfo(
            handle.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 {
        return Err(NotPrivate::Unreadable(io::Error::from_raw_os_error(
            rc as i32,
        )));
    }
    // SAFETY: `owner` and `dacl` point into `sd`, alive until the free.
    let result = unsafe { judge(&me, owner, dacl, others) };
    unsafe { LocalFree(sd) };
    result
}

/// # Safety
/// `owner` and `dacl` are null or point at a live SID and ACL.
unsafe fn judge(me: &Sid, owner: PSID, dacl: *mut ACL, others: Others) -> Result<(), NotPrivate> {
    let owner = unsafe { Sid::copy(owner) };
    if !owner.as_ref().is_some_and(|o| may_own(o, me)) {
        return Err(NotPrivate::Owner {
            owner: owner.map_or_else(|| "no account".to_string(), |o| account_name(&o)),
            me: account_name(me),
        });
    }
    if dacl.is_null() {
        return Err(NotPrivate::NoDacl);
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: `size` is the struct the class names, at its own size.
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(NotPrivate::Unreadable(io::Error::last_os_error()));
    }
    let refused = match others {
        Others::Nothing => READS | WRITES,
        Others::ReadOnly => WRITES,
    };
    let mut who = Vec::new();
    let mut change = false;
    for i in 0..size.AceCount {
        let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `i` is below the ACL's entry count.
        if unsafe { GetAce(dacl, i, &mut ace) } == 0 || ace.is_null() {
            return Err(NotPrivate::Unreadable(io::Error::last_os_error()));
        }
        // SAFETY: every entry starts with an ACE_HEADER.
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        // An inherit-only entry (INHERIT_ONLY_ACE) is judged as if it
        // applied to the object too: conservative (crate doc).
        match entry(u32::from(header.AceType)) {
            Entry::Allow => {}
            Entry::GrantsNothing => continue,
            Entry::Unknown => return Err(NotPrivate::UnknownEntry(header.AceType)),
        }
        // SAFETY: an allowed (or allowed-callback) entry is laid out as
        // ACCESS_ALLOWED_ACE, its SID starting at `SidStart`.
        let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
        let sid_ptr = std::ptr::addr_of!(allowed.SidStart) as PSID;
        let Some(sid) = (unsafe { Sid::copy(sid_ptr) }) else {
            return Err(NotPrivate::UnknownEntry(header.AceType));
        };
        if sid == *me || TRUSTED.contains(&sid.string().as_str()) {
            continue;
        }
        if allowed.Mask & refused != 0 {
            change |= allowed.Mask & WRITES != 0;
            let name = account_name(&sid);
            if !who.contains(&name) {
                who.push(name);
            }
        }
    }
    if who.is_empty() {
        Ok(())
    } else {
        Err(NotPrivate::Others { who, change })
    }
}

/// [`check_handle`] on what is at `path`, opened without following a link
/// (a link there is [`NotPrivate::Link`]); a directory is opened too.
pub fn check_path(path: &Path, others: Others) -> Result<Found, NotPrivate> {
    let file = super::create::open_meta(path).map_err(|e| match e {
        super::OpenError::Link => NotPrivate::Link,
        super::OpenError::Io(e) => NotPrivate::Unreadable(e),
    })?;
    let is_dir = file.metadata().map_err(NotPrivate::Unreadable)?.is_dir();
    use std::os::windows::io::AsHandle;
    check_handle(file.as_handle(), others)?;
    Ok(Found { is_dir })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_account_system_and_the_administrators_may_own_it() {
        let me = Sid::parse("S-1-5-21-1-2-3-1001");
        assert!(may_own(&me, &me));
        assert!(may_own(&Sid::parse("S-1-5-18"), &me));
        assert!(may_own(&Sid::parse("S-1-5-32-544"), &me));
        // Another user, Users, and the owner stand-ins are no owners.
        assert!(!may_own(&Sid::parse("S-1-5-21-1-2-3-1002"), &me));
        assert!(!may_own(&Sid::parse("S-1-5-32-545"), &me));
        assert!(!may_own(&Sid::parse("S-1-3-0"), &me));
    }

    #[test]
    fn only_known_kinds_that_grant_nothing_are_let_be() {
        assert_eq!(entry(0), Entry::Allow);
        assert_eq!(entry(9), Entry::Allow);
        for deny in [1, 6, 10, 12] {
            assert_eq!(entry(deny), Entry::GrantsNothing, "{deny}");
        }
        for sacl in [2, 3, 7, 8, 13, 14, 15, 16, 17, 18, 19, 20, 21] {
            assert_eq!(entry(sacl), Entry::GrantsNothing, "{sacl}");
        }
        // Compound, object and callback-object allow entries, and kinds
        // past the known ones.
        for unknown in [4, 5, 11, 22, 200] {
            assert_eq!(entry(unknown), Entry::Unknown, "{unknown}");
        }
    }
}
