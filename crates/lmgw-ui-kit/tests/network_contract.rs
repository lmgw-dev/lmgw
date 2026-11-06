//! The kit's network contract (personality-profiles design §5): it reaches the
//! gateway only through `/chat/api/profiles*`, `/chat/api/tools*` (the tool
//! picker, chat-plane design "WP3b"), `/v1/models` and `/v1/audio/voices`,
//! relative to the base the host configures, and it knows no Tauri and no
//! router. A client app proxies exactly those prefixes, so a new route in the
//! kit must be a conscious change to this file.

use std::fs;
use std::path::{Path, PathBuf};

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

#[test]
fn the_kit_calls_no_admin_route_and_knows_no_shell_or_router() {
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(!files.is_empty());
    let mut hits = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let lower = line.to_lowercase();
            if lower.contains("tauri") || lower.contains("leptos_router") {
                hits.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
            }
            // `/api/` is fine only as part of `/chat/api/`.
            let mut rest = line;
            while let Some(i) = rest.find("/api/") {
                if !rest[..i].ends_with("/chat") {
                    hits.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
                    break;
                }
                rest = &rest[i + 5..];
            }
        }
    }
    assert!(
        hits.is_empty(),
        "outside the kit's contract:\n{}",
        hits.join("\n")
    );
}

/// The kit's widgets reach lmgw through the transport their host hands them
/// (`http::use_http`, chat-plane design "WP3a: the transport seam"), never
/// through the free functions, which are the browser's `fetch`: a picker in
/// an extension sidebar has no `fetch` to lmgw. Only the HTTP client itself
/// (`src/http.rs`, `src/http/`) may name them.
#[test]
fn the_kit_s_widgets_use_the_handed_transport() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    let free = [
        "http::get(",
        "http::get::<",
        "http::get_text(",
        "http::post(",
        "http::post::<",
        "http::post_raw",
        "http::post_bytes(",
        "http::post_no_content(",
        "gloo_net::",
    ];
    let mut hits = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&src).unwrap();
        if rel == Path::new("http.rs") || rel.starts_with("http") {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if free.iter().any(|f| code.contains(f)) {
                hits.push(format!("{}:{}: {}", rel.display(), n + 1, code));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "kit code calling lmgw past the handed transport:\n{}",
        hits.join("\n")
    );
}
