//! Windows (Windows design §4.4): a link handed to the shell's `open` verb
//! (`ShellExecuteW`), which starts the scheme's registered handler, the
//! default browser for a web link.
//!
//! **Not through lmgw's command line, but into the handler's.** No program of
//! lmgw's own parses the URL (`explorer.exe` would read a comma in it as a
//! switch), but the handler's registered command is a line like
//! `"C:\…\mail.exe" /m "%1"`, and Windows pastes the URL into `%1` as it
//! is. A web URL as `url` serializes it has no `"`, whitespace or control
//! character left; an opaque one (a `mailto:` link's path) keeps a literal
//! `"` and spaces, and a `"` there would end the handler's argument and start
//! new ones. A `\` is the other half of that: before the handler's closing
//! quote (`"%1"` with a URL ending in `\`) it makes `\"`, which the
//! handler's argument parsing reads as a literal quote, and the argument runs
//! on. So those characters are percent-encoded first ([`shell_safe`]), which a
//! `mailto:` handler decodes back (RFC 6068 allows any character
//! percent-encoded); in a web URL's query `%5C` reads as `\`.
//!
//! On a thread of its own with COM set up as the shell asks, so the UI thread
//! does not wait for the handler; a failure is logged there with
//! `ShellExecuteW`'s own answer ([`shell_execute_error`]: it returns its
//! code rather than setting the thread's last error), without the URL (the
//! tray's carries a login nonce, a pairing link a credential). The handler
//! inherits lmgw's environment as it is, the stream identity variables
//! included: the shell takes no environment of its own, and those are
//! libpulse's and libpipewire's, which nothing on Windows reads.

use std::borrow::Cow;

/// Hand `url` to the shell's `open` verb (module doc).
#[cfg(windows)]
pub(crate) fn open(url: &str) -> std::io::Result<()> {
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let (verb, file) = (wide("open"), wide(&shell_safe(url)));
    std::thread::Builder::new()
        .name("lmgw-open-url".into())
        .spawn(move || {
            // SAFETY: COM is set up for this thread and torn down on it; the
            // strings are NUL-terminated and alive for the call.
            let rc = unsafe {
                let com = CoInitializeEx(
                    std::ptr::null(),
                    (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
                );
                let rc = ShellExecuteW(
                    std::ptr::null_mut(),
                    verb.as_ptr(),
                    file.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    SW_SHOWNORMAL,
                ) as isize;
                if com >= 0 {
                    CoUninitialize();
                }
                rc
            };
            if let Some(why) = shell_execute_error(rc) {
                tracing::warn!("Windows did not open a link (ShellExecuteW answered {rc}: {why})");
            }
        })
        .map(drop)
}

/// `url` with every `"`, `\`, whitespace and control character
/// percent-encoded (its UTF-8 bytes), the rest as it is (module doc).
#[cfg(any(windows, test))]
pub(crate) fn shell_safe(url: &str) -> Cow<'_, str> {
    let unsafe_char = |c: char| c == '"' || c == '\\' || c.is_whitespace() || c.is_control();
    if !url.contains(unsafe_char) {
        return Cow::Borrowed(url);
    }
    let mut out = String::with_capacity(url.len() + 8);
    for c in url.chars() {
        if unsafe_char(c) {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// What `ShellExecuteW`'s answer `rc` means: `None` above 32 (it worked),
/// else the documented reason, or the bare number for one it does not list.
#[cfg(any(windows, test))]
pub(crate) fn shell_execute_error(rc: isize) -> Option<Cow<'static, str>> {
    let why = match rc {
        33.. => return None,
        0 => "the system is out of memory or resources",
        2 => "the file was not found",
        3 => "the path was not found",
        5 => "access was denied",
        8 => "there was not enough memory",
        11 => "the program is not a valid executable",
        26 => "a sharing violation occurred",
        27 => "the link's association is incomplete or invalid",
        28 => "the DDE request timed out",
        29 => "the DDE request failed",
        30 => "the DDE request could not run, other DDE requests were running",
        31 => "no application is associated with this kind of link",
        32 => "a library the handler needs was not found",
        other => {
            return Some(Cow::Owned(format!(
                "an error the documentation does not list ({other})"
            )))
        }
    };
    Some(Cow::Borrowed(why))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_whitespace_and_control_characters_are_percent_encoded() {
        // What `url` hands on for a mailto: link: its opaque path keeps them.
        let u: tauri::Url = "mailto:a\"b@c.example /d".parse().unwrap();
        assert!(u.as_str().contains(['"', ' ']), "the premise: {u}");
        assert_eq!(shell_safe(u.as_str()), "mailto:a%22b@c.example%20/d");
        assert_eq!(
            shell_safe("mailto:a\"b c\td\u{7f}\u{2028}e"),
            "mailto:a%22b%20c%09d%7F%E2%80%A8e"
        );
        // A web URL as `url` serializes it has none of them, and passes as it is.
        let web: tauri::Url = "https://example.org/a b?q=\"x\"#f g".parse().unwrap();
        assert!(
            matches!(shell_safe(web.as_str()), Cow::Borrowed(_)),
            "{web}"
        );
        assert_eq!(
            shell_safe("https://example.org/%20"),
            "https://example.org/%20"
        );
    }

    #[test]
    fn a_backslash_is_percent_encoded_so_the_handlers_closing_quote_stays_one() {
        // `"%1"` with a URL ending in `\` would end in `\"`: an escaped quote.
        let u: tauri::Url = "mailto:a@b.example?body=c:\\dir\\".parse().unwrap();
        assert!(u.as_str().ends_with('\\'), "the premise: {u}");
        let safe = shell_safe(u.as_str());
        assert_eq!(safe, "mailto:a@b.example?body=c:%5Cdir%5C");
        assert!(!format!("\"{safe}\"").contains("\\\""));
        // In a web URL's query, too (its path has none: `url` makes them `/`).
        let web: tauri::Url = "https://example.org/a\\b?q=x\\".parse().unwrap();
        assert_eq!(shell_safe(web.as_str()), "https://example.org/a/b?q=x%5C");
    }

    #[test]
    fn shell_execute_answers_are_read_from_its_return_code() {
        assert_eq!(shell_execute_error(42), None);
        assert_eq!(shell_execute_error(33), None);
        assert_eq!(
            shell_execute_error(31).unwrap(),
            "no application is associated with this kind of link"
        );
        assert_eq!(shell_execute_error(2).unwrap(), "the file was not found");
        assert_eq!(
            shell_execute_error(17).unwrap(),
            "an error the documentation does not list (17)"
        );
    }
}
