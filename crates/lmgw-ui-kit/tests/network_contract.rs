//! The kit's network contract (personality-profiles design §5): it reaches the
//! gateway only through `/chat/api/profiles*`, `/v1/models` and
//! `/v1/audio/voices`, relative to the base the host configures, and it knows
//! no Tauri and no router. A client app proxies exactly those prefixes, so a
//! new route in the kit must be a conscious change to this file.

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
