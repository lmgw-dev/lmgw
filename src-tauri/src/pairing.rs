//! A device's pairing link, clicked in lmgw's own window (client-apps design
//! §1.4).
//!
//! WebKitGTK never launches a URL-scheme handler by itself, so a click on the
//! Devices card's `lmgw-pair:` link would do nothing in the app window. The
//! shell hands the link to the desktop (`xdg-open`, which starts the client
//! registered for the scheme) and cancels the navigation in the webview.
//!
//! **Only the link this process just minted, once** (review W2-1). The
//! window also hosts the Chat's HTML preview, whose scripts may navigate
//! their own frame, and the navigation handler cannot tell a frame from the
//! page. A paired device holds `Chat` and its own key, so "a link carrying
//! some device's key" would let any device put a link with an address of
//! its choosing in front of the owner. So a link is handed on only when it
//! is exactly one `key_create` or `key_rotate` minted in this process and
//! has not handed off yet (`devices::MintedLinks`): a link replayed from a
//! preview can then only be the owner's own fresh one, to the address the
//! owner chose. Every other `lmgw-pair:` navigation is cancelled and dropped.
//!
//! And only to a registered handler: with no client registered for the
//! scheme, a generic `xdg-open` may fall back to a browser, and the link
//! carries a credential (review W2-17). Nothing is opened then.

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
}

/// Whether the desktop has a client registered for `lmgw-pair:` links.
pub fn handler_registered() -> bool {
    std::process::Command::new("xdg-mime")
        .args(["query", "default", "x-scheme-handler/lmgw-pair"])
        .output()
        .map(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[tokio::test]
    async fn only_the_link_just_minted_is_handed_on_and_only_once() {
        let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
        let link = |url: &str, key: &str| -> tauri::Url {
            format!("lmgw-pair:?v=1&url={url}&name=desk&key={key}")
                .parse()
                .unwrap()
        };
        // Registered as `key_create` registers it — `pairing_link`'s own
        // string — and navigated in the form WebKit hands the handler, the
        // parsed one (review W3-15): a normalisation between the two would
        // drop the owner's own link, and this would say so.
        let raw =
            lmgw_core::devices::pairing_link("http://127.0.0.1:8001", "desk", "lmgw-device-real");
        state.devices.minted.register(7, &raw);
        let minted: tauri::Url = raw.parse().unwrap();
        assert_eq!(
            minted,
            link("http%3A%2F%2F127.0.0.1%3A8001", "lmgw-device-real")
        );

        let opened = RefCell::new(Vec::<String>::new());
        let open = |u: &str| opened.borrow_mut().push(u.to_string());
        assert!(is_pairing_link(&minted));

        // The same key at another address — what a device's own key in a
        // preview would carry — and a key nobody minted: dropped.
        for url in [
            link("https%3A%2F%2Fevil.example", "lmgw-device-real"),
            link("http%3A%2F%2F127.0.0.1%3A8001", "lmgw-device-forged"),
            "lmgw-pair:?v=1&url=http%3A%2F%2Fevil".parse().unwrap(),
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
}
