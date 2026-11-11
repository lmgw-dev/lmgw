//! A device's pairing link, clicked in lmgw's own window (client-apps design
//! §1.4).
//!
//! WebKitGTK never launches a URL-scheme handler by itself, so a click on the
//! Devices card's `lmgw-pair:` link would do nothing in the app window. The
//! shell hands the link to the desktop (`xdg-open`, which starts the client
//! registered for the scheme; on Windows the shell's `open` verb) and cancels
//! the navigation in the webview.
//!
//! **Only the link this process just minted, once** (review W2-1). The
//! window also hosts the Chat's HTML preview, whose scripts may navigate
//! their own frame, and the navigation handler cannot tell a frame from the
//! page. A paired device holds `Chat`, so "a link that looks like a pairing
//! link" would let any device put a link with an address of its choosing in
//! front of the owner. So a link is handed on only when it is exactly one
//! `key_create` or `device_token` minted in this process, has not handed
//! off yet and its token lives (`devices::MintedLinks`): a link replayed
//! from a preview can then only be the owner's own fresh one, to the
//! addresses the owner chose. Every other `lmgw-pair:` navigation is
//! cancelled and dropped.
//!
//! And only to a registered handler: with no client registered for the
//! scheme, a generic `xdg-open` may fall back to a browser, and the link
//! carries a credential (review W2-17). Nothing is opened then. Linux asks
//! `xdg-mime`, Windows the shell's association (`AssocQueryStringW`).

use lmgw_core::state::SharedState;

/// The scheme of a pairing link.
pub const SCHEME: &str = "lmgw-pair";

/// Is `url` a pairing link? The navigation handler asks first.
pub fn is_pairing_link(url: &tauri::Url) -> bool {
    url.scheme() == SCHEME
}

/// Hand a pairing link to the desktop when this process minted exactly it
/// and has not handed it off yet; drop it, with a log line, otherwise.
pub fn hand_off(url: &tauri::Url, state: &SharedState, open: impl Fn(&str)) {
    if !state.devices.minted.take(url.as_str()) {
        // The link itself carries a credential: never logged (§1.4).
        tracing::warn!(
            "a pairing link this gateway did not just mint (or already handed off) was not opened"
        );
        return;
    }
    open(url.as_str());
    // The audit log's entry while accounts are on (headless server design
    // §5.8): the device's name the link carries, never the link or its
    // token.
    let mut target = serde_json::Map::new();
    if let Some((_, name)) = url.query_pairs().find(|(k, _)| k == "name") {
        target.insert("name".into(), name.into_owned().into());
    }
    let state = state.clone();
    tauri::async_runtime::spawn(async move {
        lmgw_core::audit::desktop(
            &state,
            lmgw_core::audit::Via::App,
            "pairing_link_open",
            target,
            200,
        )
        .await;
    });
}

/// Whether the desktop has a client registered for `lmgw-pair:` links: the
/// default handler `xdg-mime` names for the scheme.
#[cfg(not(windows))]
pub fn handler_registered() -> bool {
    std::process::Command::new("xdg-mime")
        .args(["query", "default", "x-scheme-handler/lmgw-pair"])
        .output()
        .map(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

/// Windows (Windows design §4.4): whether a program is registered for the
/// scheme, asked the way the shell resolves it ([`scheme_registered`]).
///
/// lmgw's installer registers no handler for the scheme: the link is for a
/// client (Kai), whose own installer registers it; lmgw as its handler would
/// hand the link to itself.
#[cfg(windows)]
pub fn handler_registered() -> bool {
    scheme_registered(SCHEME)
}

/// Whether a program is registered for `scheme`: `AssocQueryStringW` for the
/// protocol's `open` command (the user's `HKCU\Software\Classes`, then the
/// machine's). With `ASSOCF_INIT_IGNOREUNKNOWN` a scheme with no key of its
/// own answers `ERROR_NO_ASSOCIATION`; without it the shell falls back to its
/// `Unknown` class, whose `open` is the "open with" picker, and every scheme
/// would count as registered. No association means nothing is opened, as on
/// Linux.
#[cfg(windows)]
fn scheme_registered(scheme: &str) -> bool {
    use windows_sys::Win32::UI::Shell::{
        AssocQueryStringW, ASSOCF_INIT_IGNOREUNKNOWN, ASSOCF_IS_PROTOCOL, ASSOCSTR_COMMAND,
    };
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let (scheme, verb) = (wide(scheme), wide("open"));
    let mut len: u32 = 0;
    // SAFETY: the strings are NUL-terminated and alive for the call; with no
    // output buffer the call writes only the length the command needs.
    let hr = unsafe {
        AssocQueryStringW(
            ASSOCF_IS_PROTOCOL | ASSOCF_INIT_IGNOREUNKNOWN,
            ASSOCSTR_COMMAND,
            scheme.as_ptr(),
            verb.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
        )
    };
    // S_OK or S_FALSE (the length only) with a command longer than its NUL.
    (hr == 0 || hr == 1) && len > 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[tokio::test]
    async fn only_the_link_just_minted_is_handed_on_and_only_once() {
        let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
        let fp = format!("sha256:{}", "9f".repeat(32));
        let real = format!("lmgw-enrol-{}", "4b1d".repeat(8));
        let forged = format!("lmgw-enrol-{}", "0bad".repeat(8));
        let link = |url: &str, token: &str| -> tauri::Url {
            format!(
                "lmgw-pair:?v=2&name=desk&url={url}&fp=sha256%3A{}&token={token}",
                "9f".repeat(32)
            )
            .parse()
            .unwrap()
        };
        // Registered as `key_create` registers it — `pairing_link`'s own
        // string — and navigated in the form WebKit hands the handler, the
        // parsed one (review W3-15): a normalisation between the two would
        // drop the owner's own link, and this would say so.
        let raw = lmgw_core::devices::pairing_link(
            "desk",
            &["https://127.0.0.1:8788".to_string()],
            &fp,
            &real,
        );
        let lapses = chrono::Utc::now() + chrono::Duration::minutes(10);
        state.devices.minted.register(7, &raw, lapses);
        let minted: tauri::Url = raw.parse().unwrap();
        assert_eq!(minted, link("https%3A%2F%2F127.0.0.1%3A8788", &real));

        let opened = RefCell::new(Vec::<String>::new());
        let open = |u: &str| opened.borrow_mut().push(u.to_string());
        assert!(is_pairing_link(&minted));

        // The same token to another address — what a preview replaying the
        // owner's link would carry — and a token nobody minted: dropped.
        for url in [
            link("https%3A%2F%2Fevil.example", &real),
            link("https%3A%2F%2F127.0.0.1%3A8788", &forged),
            "lmgw-pair:?v=2&url=https%3A%2F%2Fevil".parse().unwrap(),
        ] {
            hand_off(&url, &state, open);
        }
        assert!(opened.borrow().is_empty(), "{:?}", opened.borrow());

        // The minted one, once.
        hand_off(&minted, &state, open);
        hand_off(&minted, &state, open);
        assert_eq!(opened.borrow().as_slice(), [minted.as_str()]);

        assert!(!is_pairing_link(&"http://127.0.0.1:8001/".parse().unwrap()));
    }

    /// A throwaway per-user scheme: unregistered, then with a handler under
    /// `HKCU\Software\Classes` (an exe that exists), then removed again.
    #[cfg(windows)]
    #[test]
    fn a_scheme_counts_as_registered_only_with_a_handler_of_its_own() {
        let scheme = format!("lmgw-test-assoc-{}", std::process::id());
        let key = format!(r"HKCU\Software\Classes\{scheme}");
        let reg = |args: &[&str]| {
            let out = std::process::Command::new("reg")
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "reg {args:?}: {out:?}");
        };
        assert!(!scheme_registered(&scheme), "no key: no handler");
        reg(&["add", &key, "/v", "URL Protocol", "/d", "", "/f"]);
        let command = format!(r"{key}\shell\open\command");
        let exe = r#""C:\Windows\System32\whoami.exe" "%1""#;
        reg(&["add", &command, "/ve", "/d", exe, "/f"]);
        let registered = scheme_registered(&scheme);
        reg(&["delete", &key, "/f"]);
        assert!(registered, "a per-user handler counts");
        assert!(!scheme_registered(&scheme), "removed again");
    }
}
