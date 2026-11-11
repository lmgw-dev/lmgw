//! The Windows install step: the per-user NSIS installer Tauri's bundler makes, started silently
//! for the current user.
//!
//! The installer replaces the running program's files, so the program cannot wait for it: the
//! caller ends its own work, starts the installer ([`spawn_installer`]) and exits. With
//! [`INSTALLER_ARGS`] the installer asks nothing (`/S`) and needs no administrator rights (it
//! installs for the current user only); a copy of the program that still runs is closed through
//! the Restart Manager; the files are overwritten; no shortcut is made again and no WebView2
//! bootstrapper runs (`/UPDATE`); and the installed program is started again (`/R`). Nothing of
//! that is reported back: the program that starts again compares its own version with the one
//! it went to install, which is the caller's to record.
//!
//! **Only the installer's own copy updates itself** ([`can_self_install`]). The installer records
//! its directory under the per-user uninstall key. A copy that runs from anywhere else (a build
//! directory, a copied folder) would get a new copy installed beside it while it keeps running
//! the old one, so a caller offers no install there, as on a Linux host without dnf.
//!
//! **Integrity.** A manifest's `nsis` entry is refused unless its sha256 is 64 hex digits and its
//! URL https, or http to this machine (a loopback feed, as a local check serves) ([`check_entry`]):
//! the RPM's empty sha256 skips the check after the download, the installer's never does. The
//! installer is the file [`crate::download`] wrote and checked against that sha256, in
//! [`crate::private_dir`], and [`spawn_installer`] hashes it again right before the start, through
//! a handle that lets nobody write, rename or delete the file until the installer has been
//! started: a mismatch removes it and starts nothing. That keeps a swapped file out, not a
//! same-user process that steers lmgw itself. It is not signed yet, so Windows' SmartScreen does
//! not know its publisher.

use std::fmt;
#[cfg(any(windows, test))]
use std::io::Read;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::Command;

#[cfg(any(windows, test))]
use sha2::{Digest, Sha256};

use crate::manifest::Artifact;

/// Why a manifest's `nsis` entry is refused ([`check_entry`]); displays as what is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryError {
    /// The sha256 is empty: the installer would run unchecked.
    NoSha256,
    /// The sha256 is not 64 hex digits (its length here).
    BadSha256 { len: usize },
    /// The URL does not parse.
    BadUrl(String),
    /// The URL's scheme is neither https nor http to this machine.
    NotHttps { scheme: String },
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryError::NoSha256 => f.write_str("its sha256 is empty"),
            EntryError::BadSha256 { len } => write!(
                f,
                "its sha256 is not 64 hex digits ({len} characters, or not all hex)"
            ),
            EntryError::BadUrl(e) => write!(f, "its URL is not a URL ({e})"),
            EntryError::NotHttps { scheme } => write!(
                f,
                "its URL is {scheme}:, not https: (http: only to this machine, 127.0.0.1, ::1 or \
                 localhost)"
            ),
        }
    }
}

impl std::error::Error for EntryError {}

/// Whether a manifest's `nsis` entry may be downloaded and run: its sha256 64 hex digits (any
/// case, surrounding whitespace ignored, as [`crate::download`] compares it), its URL https, or
/// http to a loopback host (`127.0.0.1`, `::1`, `localhost`: a feed this machine serves).
pub fn check_entry(entry: &Artifact) -> Result<(), EntryError> {
    let sha = entry.sha256.trim();
    if sha.is_empty() {
        return Err(EntryError::NoSha256);
    }
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(EntryError::BadSha256 {
            len: sha.chars().count(),
        });
    }
    let url = reqwest::Url::parse(&entry.url).map_err(|e| EntryError::BadUrl(e.to_string()))?;
    let loopback = url.host_str().is_some_and(|h| {
        h.eq_ignore_ascii_case("localhost")
            || h.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    match url.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        scheme => Err(EntryError::NotHttps {
            scheme: scheme.to_string(),
        }),
    }
}

/// The hex sha256 of what `r` reads, when it is not `expected` (compared as [`crate::download`]
/// compares it); `None` when it is.
#[cfg(any(windows, test))]
fn mismatch(r: &mut impl Read, expected: &str) -> std::io::Result<Option<String>> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hex::encode(hasher.finalize());
    Ok((!got.eq_ignore_ascii_case(expected.trim())).then_some(got))
}

/// The installer's switches: silent, an update, the program started again when it is done.
pub const INSTALLER_ARGS: [&str; 3] = ["/S", "/UPDATE", "/R"];

/// The installer's command: `<installer> /S /UPDATE /R`. `installer` is the path the caller
/// downloaded to, never one built from the manifest.
pub fn installer_command(installer: &Path) -> Command {
    let mut cmd = Command::new(installer);
    cmd.args(INSTALLER_ARGS);
    cmd
}

/// Start the installer ([`installer_command`]) and return without waiting for it: it has to
/// outlive the caller, which exits next. Right before the start the file is hashed again against
/// `sha256`, the manifest's, through a handle that shares reading only, so the file cannot be
/// written, renamed or deleted until the installer has been started; on a mismatch the file is
/// removed, nothing starts, and the error (`InvalidData`) says so. Detached from the caller's
/// console and process group, with no standard handles, and out of the caller's job when that
/// job allows it (a job that does not answers the request with "access denied", and the
/// installer then runs in it: one that kills its processes when it closes would end the
/// installer with it).
#[cfg(windows)]
pub fn spawn_installer(installer: &Path, sha256: &str) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
    };
    let mut held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(installer)?;
    if let Some(got) = mismatch(&mut held, sha256)? {
        drop(held);
        let removed = match std::fs::remove_file(installer) {
            Ok(()) => "removed".to_string(),
            Err(e) => format!("not removed ({e})"),
        };
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "its sha256 changed after the download (expected {}, got {got}), so it was not \
                 run; the file is {removed}",
                sha256.trim()
            ),
        ));
    }
    let spawn = |flags: u32| {
        installer_command(installer)
            .creation_flags(flags)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(drop)
    };
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    let started = match spawn(base | CREATE_BREAKAWAY_FROM_JOB) {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => spawn(base),
        other => other,
    };
    drop(held);
    started
}

/// The per-user uninstall key the installer writes for `product` (under `HKEY_CURRENT_USER`).
pub fn uninstall_key(product: &str) -> String {
    format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{product}")
}

/// Whether the running program is the copy the per-user installer put in place: `Ok` with the
/// install directory, or why not, as a sentence naming the key or both paths.
#[cfg(windows)]
pub fn can_self_install(product: &str) -> Result<PathBuf, String> {
    let key = uninstall_key(product);
    let Some(dir) = installed_dir(&key) else {
        return Err(format!(
            "no installation of {product} for this account is recorded (HKEY_CURRENT_USER\\{key} \
             has no InstallLocation)"
        ));
    };
    let exe = std::env::current_exe().map_err(|e| format!("the running program's path: {e}"))?;
    if runs_from(&dir, &exe) {
        Ok(dir)
    } else {
        Err(format!(
            "this {} is not the installed copy, which is in {}",
            exe.display(),
            dir.display()
        ))
    }
}

/// The `InstallLocation` value of the uninstall key `key` under `HKEY_CURRENT_USER`, its quotes
/// taken off ([`unquote`]).
#[cfg(windows)]
fn installed_dir(key: &str) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let (key, value) = (wide(key), wide("InstallLocation"));
    let mut bytes: u32 = 0;
    // SAFETY: the strings are NUL-terminated and alive for both calls; the first call asks for
    // the size only (no buffer), the second writes at most `bytes` into a buffer of that size.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
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
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
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
    let s = std::ffi::OsString::from_wide(&buf);
    let s = s.to_string_lossy();
    let dir = unquote(&s);
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

/// A registry path value without the one pair of double quotes Tauri's installer writes around
/// it (`"C:\…\lmgw"`), and without surrounding whitespace.
pub fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

/// Whether `exe` sits directly in `dir`, both resolved (links, `.` and `..`) and compared
/// without case, as Windows compares paths. A path that cannot be resolved is not in it.
pub fn runs_from(dir: &Path, exe: &Path) -> bool {
    let (Ok(dir), Some(Ok(parent))) = (
        std::fs::canonicalize(dir),
        exe.parent().map(std::fs::canonicalize),
    ) else {
        return false;
    };
    let (a, b) = (dir.to_string_lossy(), parent.to_string_lossy());
    if cfg!(windows) {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    #[test]
    fn the_installer_runs_silent_as_an_update_and_restarts_the_program() {
        let cmd = installer_command(Path::new("update-setup.exe"));
        assert_eq!(cmd.get_program(), OsStr::new("update-setup.exe"));
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(args, ["/S", "/UPDATE", "/R"].map(OsStr::new));
    }

    fn entry(url: &str, sha256: &str) -> Artifact {
        Artifact {
            file: "app_1.0.0_x64-setup.exe".into(),
            url: url.into(),
            sha256: sha256.into(),
        }
    }

    #[test]
    fn an_nsis_entry_needs_a_full_sha256_and_https() {
        let sha = "aB".repeat(32);
        let ok = |url: &str| check_entry(&entry(url, &sha));
        assert_eq!(ok("https://example.org/a.exe"), Ok(()));
        assert_eq!(
            ok("http://127.0.0.1:18402/a.exe"),
            Ok(()),
            "a loopback feed"
        );
        assert_eq!(ok("http://localhost/a.exe"), Ok(()));
        assert_eq!(ok("http://[::1]:8/a.exe"), Ok(()));
        assert_eq!(
            check_entry(&entry("https://example.org/a.exe", &format!(" {sha}\n"))),
            Ok(()),
            "whitespace around it, as the download compares it"
        );
        let refused = |url: &str, sha: &str| check_entry(&entry(url, sha)).unwrap_err();
        let e = refused("https://example.org/a.exe", "");
        assert_eq!(e, EntryError::NoSha256);
        assert_eq!(e.to_string(), "its sha256 is empty");
        assert_eq!(
            refused("https://example.org/a.exe", "  "),
            EntryError::NoSha256
        );
        let e = refused("https://example.org/a.exe", &"a".repeat(63));
        assert_eq!(e, EntryError::BadSha256 { len: 63 });
        assert!(e.to_string().contains("not 64 hex digits"), "{e}");
        assert_eq!(
            refused("https://example.org/a.exe", &"g".repeat(64)),
            EntryError::BadSha256 { len: 64 }
        );
        let e = refused("http://example.org/a.exe", &sha);
        assert_eq!(
            e,
            EntryError::NotHttps {
                scheme: "http".into()
            }
        );
        assert!(
            e.to_string().starts_with("its URL is http:, not https:"),
            "{e}"
        );
        assert!(matches!(
            refused("http://127.0.0.2.example/a.exe", &sha),
            EntryError::NotHttps { .. }
        ));
        assert!(matches!(
            refused("file:///C:/a.exe", &sha),
            EntryError::NotHttps { .. }
        ));
        assert!(matches!(refused("a.exe", &sha), EntryError::BadUrl(_)));
    }

    #[test]
    fn the_second_hash_tells_the_downloaded_file_from_another() {
        let data = b"the installer";
        let sha = hex::encode(Sha256::digest(data));
        assert_eq!(mismatch(&mut &data[..], &sha).unwrap(), None);
        assert_eq!(mismatch(&mut &data[..], &sha.to_uppercase()).unwrap(), None);
        let other = mismatch(&mut &b"another"[..], &sha).unwrap().unwrap();
        assert_eq!(other, hex::encode(Sha256::digest(b"another")));
    }

    /// The start through the held handle, and a file changed after the download: a copy of
    /// `whoami.exe` (which ends at once on the installer's switches) as the installer.
    #[cfg(windows)]
    #[test]
    fn the_installer_starts_only_with_the_downloaded_hash() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("setup.exe");
        std::fs::copy(r"C:\Windows\System32\whoami.exe", &exe).unwrap();
        let sha = hex::encode(Sha256::digest(std::fs::read(&exe).unwrap()));
        spawn_installer(&exe, &sha).unwrap();
        // It has started; whoami may still hold its image, so the copy is a new one.
        let swapped = dir.path().join("swapped.exe");
        std::fs::copy(r"C:\Windows\System32\whoami.exe", &swapped).unwrap();
        let e = spawn_installer(&swapped, &"0".repeat(64)).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        let msg = e.to_string();
        assert!(msg.contains("changed after the download"), "{msg}");
        assert!(msg.ends_with("the file is removed"), "{msg}");
        assert!(!swapped.exists(), "removed, not run");
    }

    #[test]
    fn a_quoted_install_location_is_unquoted_once() {
        assert_eq!(
            unquote(r#""C:\Users\a\AppData\Local\Programs\lmgw""#),
            r"C:\Users\a\AppData\Local\Programs\lmgw"
        );
        assert_eq!(unquote(r"C:\x"), r"C:\x");
        assert_eq!(unquote(r#"  "C:\x"  "#), r"C:\x");
        assert_eq!(unquote(r#""C:\x"#), r#""C:\x"#, "an unbalanced quote stays");
        assert_eq!(
            uninstall_key("lmgw"),
            r"Software\Microsoft\Windows\CurrentVersion\Uninstall\lmgw"
        );
    }

    #[test]
    fn a_program_runs_from_its_own_directory_only() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("Programs").join("app");
        std::fs::create_dir_all(&dir).unwrap();
        let other = base.path().join("build");
        std::fs::create_dir_all(&other).unwrap();
        assert!(runs_from(&dir, &dir.join("app.exe")));
        assert!(runs_from(&dir, &dir.join("..").join("app").join("app.exe")));
        assert!(!runs_from(&dir, &other.join("app.exe")));
        assert!(
            !runs_from(&dir, &dir.join("sub").join("app.exe")),
            "a subdirectory is not it"
        );
        assert!(!runs_from(&base.path().join("gone"), &dir.join("app.exe")));
        if cfg!(windows) {
            let upper = PathBuf::from(dir.to_string_lossy().to_uppercase());
            assert!(
                runs_from(&upper, &dir.join("app.exe")),
                "Windows compares without case"
            );
        }
    }
}
