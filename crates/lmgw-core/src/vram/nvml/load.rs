//! Where NVML's library is loaded from.
//!
//! Linux: `libnvidia-ml.so.1` through the dynamic loader's search, as
//! nvml-wrapper does by default. Windows: `nvml.dll` by its full path, from
//! where the NVIDIA driver installs it: the system directory (every DCH
//! driver; checked 2026-11-10 with a 560-series driver, `C:\Windows\System32\nvml.dll`),
//! then `%ProgramFiles%\NVIDIA Corporation\NVSMI\` (where older standard
//! drivers put it). A full path keeps the application directory and the
//! working directory out of the search for `nvml.dll` itself.
//!
//! Its static imports would still follow the default search order (which
//! has the application directory first) if nvml-wrapper loaded it:
//! libloading's `Library::new` is `LoadLibraryExW(path, NULL, 0)`, and
//! nvml-wrapper 0.12 takes only a path, no library loaded beforehand. So lmgw
//! loads it first, with `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR |
//! LOAD_LIBRARY_SEARCH_SYSTEM32` (its static imports from its own directory
//! and the system directory only), and nvml-wrapper's load by the same full
//! path then gets that module, already bound. lmgw's reference is released
//! once nvml-wrapper holds its own. The flags bind only the static imports:
//! a delay-loaded import, or a `LoadLibrary` call inside `nvmlInit` itself,
//! still follows the default search order.

use nvml_wrapper::Nvml;

#[cfg(not(windows))]
pub(super) fn init() -> Result<Nvml, String> {
    Nvml::init().map_err(|e| e.to_string())
}

#[cfg(windows)]
pub(super) fn init() -> Result<Nvml, String> {
    let mut tried = Vec::new();
    for path in windows_candidates() {
        if path.is_file() {
            let preloaded = preload(&path)?;
            let nvml = Nvml::builder()
                .lib_path(path.as_os_str())
                .init()
                .map_err(|e| format!("{}: {e}", path.display()));
            // SAFETY: the module handle from `preload`, released once; a
            // loaded NVML holds its own reference.
            unsafe { windows_sys::Win32::Foundation::FreeLibrary(preloaded) };
            return nvml;
        }
        tried.push(path.display().to_string());
    }
    Err(format!(
        "nvml.dll not found (looked for {}); the NVIDIA driver installs it",
        tried.join(" and ")
    ))
}

/// `path` loaded with its imports searched in its own directory and the
/// system directory only (module docs).
#[cfg(windows)]
fn preload(path: &std::path::Path) -> Result<windows_sys::Win32::Foundation::HMODULE, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{
        LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a NUL-terminated absolute path; no file handle (reserved).
    let module = unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        // SAFETY: plain call.
        let e = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(format!(
            "{}: LoadLibraryExW(LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | \
             LOAD_LIBRARY_SEARCH_SYSTEM32) failed with Win32 error {e}",
            path.display()
        ));
    }
    Ok(module)
}

#[cfg(windows)]
fn windows_candidates() -> Vec<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    let mut out = Vec::new();
    let mut buf = vec![0u16; 260];
    loop {
        // SAFETY: the buffer holds `buf.len()` UTF-16 units.
        let n = unsafe {
            windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(
                buf.as_mut_ptr(),
                buf.len() as u32,
            )
        } as usize;
        if n == 0 {
            break;
        }
        if n > buf.len() {
            // Too small: `n` is the size it needs, terminator included.
            buf.resize(n, 0);
            continue;
        }
        out.push(PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])).join("nvml.dll"));
        break;
    }
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        out.push(
            PathBuf::from(pf)
                .join("NVIDIA Corporation")
                .join("NVSMI")
                .join("nvml.dll"),
        );
    }
    out
}
