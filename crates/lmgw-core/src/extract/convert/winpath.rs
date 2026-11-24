//! The `PATH` tools are looked up on, on Windows (design §4.4).
//!
//! A Windows installer changes the registry's `Path` values, not the running
//! process's `PATH`, so a process started before `winget install pandoc`
//! never sees it. Each lookup therefore reads the machine's
//! (`HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment`) and
//! the user's (`HKCU\Environment`) `Path` again and merges them behind the
//! process's own. `%VAR%` references in those values are expanded.
//!
//! Elsewhere [`merged`] returns the process's list as it is, and
//! [`expand`] is only used for install directories that name `%VAR%`s.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// `s` with every `%NAME%` replaced by the environment variable's value; a
/// name that is not set (or an unmatched `%`) stays as written.
pub fn expand(s: &str) -> String {
    expand_with(s, |name| std::env::var(name).ok())
}

fn expand_with(s: &str, get: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find('%') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('%') {
            Some(close) if close > 0 => match get(&after[..close]) {
                Some(v) => {
                    out.push_str(&v);
                    rest = &after[close + 1..];
                }
                None => {
                    out.push('%');
                    rest = after;
                }
            },
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The process's list, then the machine's, then the user's, each directory
/// once (compared without regard to case on Windows).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn merge(process: &OsStr, machine: Option<OsString>, user: Option<OsString>) -> OsString {
    let mut seen: Vec<String> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let lists = [Some(process.to_os_string()), machine, user];
    for list in lists.iter().flatten() {
        for d in std::env::split_paths(list) {
            if d.as_os_str().is_empty() {
                continue;
            }
            let key = if cfg!(windows) {
                d.to_string_lossy().to_lowercase()
            } else {
                d.to_string_lossy().into_owned()
            };
            if !seen.contains(&key) {
                seen.push(key);
                dirs.push(d);
            }
        }
    }
    // A directory with a quote in it cannot be joined: the process's own
    // list, as it was, is the answer then.
    std::env::join_paths(dirs).unwrap_or_else(|_| process.to_os_string())
}

/// The list to look on: `process` merged with the registry's on Windows.
pub fn merged(process: &OsStr) -> OsString {
    #[cfg(windows)]
    {
        merge(process, registry::machine_path(), registry::user_path())
    }
    #[cfg(not(windows))]
    {
        process.to_os_string()
    }
}

#[cfg(windows)]
mod registry {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ,
        RRF_RT_REG_SZ,
    };

    pub fn machine_path() -> Option<OsString> {
        path_value(
            HKEY_LOCAL_MACHINE,
            "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment",
        )
    }

    pub fn user_path() -> Option<OsString> {
        path_value(HKEY_CURRENT_USER, "Environment")
    }

    /// The `Path` value of `key`. `RegGetValueW` expands a `REG_EXPAND_SZ`
    /// value's `%VAR%`s itself (no `RRF_NOEXPAND`).
    fn path_value(root: HKEY, key: &str) -> Option<OsString> {
        let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
        let (key, value) = (wide(key), wide("Path"));
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
        let mut bytes: u32 = 0;
        // SAFETY: the strings are NUL-terminated and alive for both calls;
        // the first call asks for the size only (no buffer), the second
        // writes at most `bytes` into a buffer of that size.
        let rc = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                value.as_ptr(),
                flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut bytes,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
        // SAFETY: as above.
        let rc = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                value.as_ptr(),
                flags,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        buf.truncate(bytes as usize / 2);
        while buf.last() == Some(&0) {
            buf.pop();
        }
        Some(OsString::from_wide(&buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_names_are_expanded_and_unknown_ones_stay() {
        let get = |n: &str| (n == "LOCALAPPDATA").then(|| "C:\\Users\\example".to_string());
        assert_eq!(
            expand_with("%LOCALAPPDATA%\\Pandoc", get),
            "C:\\Users\\example\\Pandoc"
        );
        assert_eq!(expand_with("%NOPE%\\x", get), "%NOPE%\\x");
        assert_eq!(expand_with("100%", get), "100%");
        assert_eq!(expand_with("a%%b", get), "a%%b");
        assert_eq!(expand_with("/usr/bin", get), "/usr/bin");
    }

    #[test]
    fn the_registry_lists_come_after_the_process_and_repeat_nothing() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let list = |parts: &[&str]| OsString::from(parts.join(sep));
        let merged = merge(
            &list(&["/p/one", "/p/two"]),
            Some(list(&["/p/two", "/m/bin"])),
            Some(list(&["/u/bin", "/m/bin"])),
        );
        let dirs: Vec<String> = std::env::split_paths(&merged)
            .map(|d| d.display().to_string())
            .collect();
        assert_eq!(dirs, ["/p/one", "/p/two", "/m/bin", "/u/bin"]);
        // No registry: the process's list.
        let only = merge(&list(&["/p/one"]), None, None);
        assert_eq!(only, list(&["/p/one"]));
    }
}
