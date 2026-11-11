//! Security identifiers: the account lmgw runs as, and the names a finding
//! shows.

use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, IsValidSid, LookupAccountSidW, TokenUser, PSID,
    SID_NAME_USE, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// A SID, copied into memory of its own.
#[derive(Clone, PartialEq, Eq)]
pub struct Sid(Vec<u8>);

impl std::fmt::Debug for Sid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string())
    }
}

impl Sid {
    /// The user SID of this process's token: the account lmgw runs as.
    pub fn current_user() -> io::Result<Self> {
        // SAFETY: the pseudo handle of the current process needs no close;
        // the token handle is closed below, once.
        let mut token: HANDLE = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut len = 0u32;
            // SAFETY: a size query: no buffer, the needed length written to `len`.
            unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
            if len == 0 {
                return Err(io::Error::last_os_error());
            }
            // u64 elements: TOKEN_USER holds a pointer, so the buffer is
            // aligned for it.
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            // SAFETY: `buf` holds at least `len` bytes, the length given.
            if unsafe {
                GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len)
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: on success the buffer starts with a TOKEN_USER whose
            // SID points into the same buffer, alive here.
            let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
            unsafe { Self::copy(user.User.Sid) }
                .ok_or_else(|| io::Error::other("the process token's user SID is not valid"))
        })();
        // SAFETY: opened above, closed once.
        unsafe { CloseHandle(token) };
        result
    }

    /// A copy of the SID at `p`; `None` for a null or invalid one.
    ///
    /// # Safety
    /// `p` is null or points at a SID that stays alive for the call.
    pub(super) unsafe fn copy(p: PSID) -> Option<Self> {
        if p.is_null() || unsafe { IsValidSid(p) } == 0 {
            return None;
        }
        let len = unsafe { GetLengthSid(p) } as usize;
        Some(Self(
            unsafe { std::slice::from_raw_parts(p as *const u8, len) }.to_vec(),
        ))
    }

    /// A pointer for the API, valid while `self` is.
    pub(super) fn as_psid(&self) -> PSID {
        self.0.as_ptr() as PSID
    }

    /// The SID written `S-1-…` in `text`; a test's fixture, so a text that
    /// is no SID panics.
    #[cfg(test)]
    pub(super) fn parse(text: &str) -> Self {
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let mut p: PSID = std::ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated; the SID is allocated by the call
        // and freed with LocalFree after the copy.
        assert!(
            unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut p) } != 0,
            "{text} is no SID: {}",
            io::Error::last_os_error()
        );
        let sid = unsafe { Self::copy(p) }.expect("a converted SID is valid");
        unsafe { LocalFree(p.cast()) };
        sid
    }

    /// The `S-1-5-…` form.
    pub fn string(&self) -> String {
        let mut out: *mut u16 = std::ptr::null_mut();
        // SAFETY: the SID is valid (only valid ones are copied); the string
        // is allocated by the call and freed with LocalFree.
        if unsafe { ConvertSidToStringSidW(self.as_psid(), &mut out) } == 0 || out.is_null() {
            return "an unreadable SID".to_string();
        }
        let text = unsafe { from_wide_ptr(out) };
        unsafe { LocalFree(out.cast()) };
        text
    }
}

/// `DOMAIN\name` of `sid` as this machine resolves it, or its `S-1-…` form
/// when it resolves to none (a deleted account, a domain not reachable).
pub fn account_name(sid: &Sid) -> String {
    let (mut name_len, mut domain_len) = (0u32, 0u32);
    let mut kind: SID_NAME_USE = 0;
    // SAFETY: a size query with no buffers.
    unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid.as_psid(),
            std::ptr::null_mut(),
            &mut name_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut kind,
        )
    };
    if name_len == 0 {
        return sid.string();
    }
    let mut name = vec![0u16; name_len as usize];
    let mut domain = vec![0u16; domain_len.max(1) as usize];
    // SAFETY: both buffers have the lengths given.
    let ok = unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid.as_psid(),
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    if ok == 0 {
        return sid.string();
    }
    let name = String::from_utf16_lossy(&name[..name_len as usize]);
    let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
    if domain.is_empty() {
        name
    } else {
        format!("{domain}\\{name}")
    }
}

/// A NUL-terminated UTF-16 string as text.
///
/// # Safety
/// `p` points at a NUL-terminated UTF-16 string.
pub(super) unsafe fn from_wide_ptr(p: *const u16) -> String {
    let mut len = 0;
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}
