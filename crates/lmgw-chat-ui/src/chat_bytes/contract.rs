//! The Chat's transport and host contract (chat-plane design, "WP3a: the
//! transport seam" and "WP3b: the lmgw-chat-ui crate"): its modules reach
//! lmgw only through the transport its host hands it, and know nothing of a
//! host but its seams (`crate::host`). No direct fetch, `EventSource` or
//! request, no kit call that takes the browser's `fetch` behind the host's
//! back, no element that loads an lmgw path by itself, and no router, address
//! bar or session of a host's own: a new one has to go through `Http`,
//! `chat_bytes` or the host, or be named here with its reason.

use std::fs;
use std::path::{Path, PathBuf};

/// What bypasses the transport or the host, and what to use instead.
const BYPASSES: &[(&str, &str)] = &[
    ("gloo_net::http", "Http::send"),
    ("EventSource", "the feed over Http (chat_live)"),
    ("web_sys::Request", "Http::send"),
    ("fetch_with", "Http::send"),
    ("lmgw_ui_kit::http::get", "http.get"),
    ("lmgw_ui_kit::http::post", "http.post"),
    ("docs::download(", "chat_bytes::Saver"),
    ("src=format!(\"/", "chat_bytes::ChatImg / ChatAudio"),
    ("href=format!(\"/chat", "chat_bytes::Saver"),
    ("src=href", "chat_bytes::ChatImg / ChatAudio"),
    (
        ".location()",
        "the host's ChatLocation, or a path relative to lmgw",
    ),
    (".history()", "ChatLocation::show_thread"),
    ("leptos_router", "ChatLocation::opened_at"),
    ("crate::session", "ChatHost::locked"),
    ("\"/voice/", "host::asset_url"),
];

/// Known exceptions: (file, the use as written, why). A use is excused when
/// its line holds the text named here, which holds the pattern it matched.
/// None since WP3b: the realtime socket's URL and the session are the
/// host's.
const ALLOWED: &[(&str, &str, &str)] = &[];

/// Whether `code` in file `rel` is an excused use of `pat`.
fn excused(rel: &str, pat: &str, code: &str) -> bool {
    ALLOWED
        .iter()
        .any(|(f, text, _)| *f == rel && text.contains(pat) && code.contains(text))
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The Chat's modules: the whole crate.
fn chat_files(src: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    sources(src, &mut files);
    files.sort();
    files
}

#[test]
fn the_chat_reaches_lmgw_only_through_its_transport() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = chat_files(&src);
    assert!(files.len() > 50, "the Chat's modules: {}", files.len());
    let mut hits = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if rel.ends_with("chat_bytes/contract.rs") {
            continue;
        }
        let text = fs::read_to_string(file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            for (pat, instead) in BYPASSES {
                if code.contains(pat) && !excused(&rel, pat, code) {
                    hits.push(format!("{rel}:{}: `{pat}` (use {instead}): {code}", n + 1));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "the Chat bypasses its transport:\n{}",
        hits.join("\n")
    );
}

#[test]
fn every_allowed_exception_is_still_there() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for (file, text, why) in ALLOWED {
        assert!(
            BYPASSES.iter().any(|(pat, _)| text.contains(pat)),
            "{file}: `{text}` excuses no pattern"
        );
        let code = fs::read_to_string(src.join(file)).unwrap();
        assert!(
            code.lines()
                .any(|l| !l.trim_start().starts_with("//") && l.contains(text)),
            "{file} no longer uses `{text}`: drop its exception ({why})"
        );
    }
}

#[test]
fn an_exception_excuses_only_its_own_use() {
    // Nothing is excused while the list is empty, whatever the line holds.
    assert!(!excused(
        "chat.rs",
        ".location()",
        "let l = window().location();"
    ));
}
