//! Serves the Leptos UI (crates/lmgw-ui, built by Trunk into its `dist/`) at
//! `/`. rust-embed embeds the bundle in release builds and reads it from disk
//! in debug builds, so the dev loop is `trunk watch` + a debug server — no
//! server rebuild per UI change.
//!
//! `/ui/*` was the SPA's mount while the old htmx UI still owned `/` (plan
//! P0–P7). It now redirects permanently to the same path at the root so
//! bookmarks, the Tauri window's stored location and any muscle-memory URL
//! keep working.

use axum::extract::Path;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;

use crate::state::SharedState;

#[derive(rust_embed::RustEmbed)]
#[folder = "../lmgw-ui/dist/"]
struct UiDist;

/// Path prefixes owned by a non-UI plane. A miss under one of these is a real
/// 404, not a client-side route — serving the SPA shell there would answer an
/// API call with HTML and hide the mistake. Prefixes end in `/` so a sibling
/// SPA route can't be swallowed (`/mcp-servers` is a page, `/mcp` is the MCP
/// protocol endpoint and matches its own route anyway).
const API_PREFIXES: [&str; 5] = [
    "api/",
    "v1/",
    "chat/api/",
    "audio-lab/api/",
    "image-lab/api/",
];

pub fn routes(state: &SharedState) -> Router<SharedState> {
    Router::new()
        .route("/", get(index))
        .route("/ui", get(redirect_legacy))
        .route("/ui/", get(redirect_legacy))
        .route("/ui/{*path}", get(redirect_legacy))
        .route("/{*path}", get(asset))
        // `Public`, and it has to be (principals §3.2): a browser with no
        // session must still be served the page that asks for one, and the
        // bundle's hashed assets are the same bytes for everyone. Phase 2's
        // login routes belong **ahead** of this router, because `/{*path}`
        // takes every path no other plane claimed.
        .route_layer(crate::server::require(state, crate::principal::Cap::Public))
}

async fn index(headers: HeaderMap) -> Response {
    serve("index.html", &headers)
}

/// 308 from the old `/ui` mount to the same path at the root, query string
/// included (`/ui/chat?t=7` → `/chat?t=7`).
async fn redirect_legacy(uri: Uri) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/ui");
    let rest = pq.strip_prefix("/ui").unwrap_or(pq);
    let target = if rest.is_empty() || rest.starts_with('?') {
        format!("/{rest}")
    } else {
        rest.to_string()
    };
    Redirect::permanent(&target).into_response()
}

/// Bundle files are served as-is; anything else without an extension is a
/// client-side route and gets the SPA's index.html.
async fn asset(Path(path): Path<String>, headers: HeaderMap) -> Response {
    if UiDist::get(&path).is_some() {
        serve(&path, &headers)
    } else if !path.contains('.') && !API_PREFIXES.iter().any(|p| path.starts_with(p)) {
        serve("index.html", &headers)
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// The dashboard's narrow Content-Security-Policy, set on every HTML response.
/// It is the enforcement of "replies never load remote images": whatever a
/// model-written reply, a Mermaid diagram or an HTML preview manages to put in
/// the DOM, the browser will not fetch images, media, fonts or stylesheets
/// from anywhere but this origin (and `data:` / `blob:`). Deliberately **not**
/// `default-src`, `script-src`, `connect-src` or `frame-src`: scripts, fetches
/// and the agent app frames must keep working. A consequence: the sandboxed
/// HTML preview of a fenced block inherits it, so remote images and
/// stylesheets inside a preview do not load.
const DASHBOARD_CSP: &str =
    "img-src 'self' data: blob:; media-src 'self' data: blob:; font-src 'self' data:; style-src 'self' 'unsafe-inline'";

/// Whether a bundle file's name carries Trunk's content hash
/// (`app-4dd5e9466ebb44f4.css`, `lmgw-ui-b5b9f9525c7eb1fc_bg.wasm`): a new
/// build gives changed bytes a new name, so such a file may be cached forever.
fn content_hashed(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some((stem, _ext)) = name.rsplit_once('.') else {
        return false;
    };
    let stem = stem.strip_suffix("_bg").unwrap_or(stem);
    stem.rsplit_once('-').is_some_and(|(_, h)| {
        h.len() == 16
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn serve(path: &str, req: &HeaderMap) -> Response {
    match UiDist::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let csp = if mime.essence_str() == "text/html" {
                DASHBOARD_CSP
            } else {
                ""
            };
            // Only a name with Trunk's content hash may be cached forever. Every
            // other file keeps its name across versions — index.html, and the
            // copied `vendor/` libraries — so it is revalidated on each load
            // against its ETag and answered 304 while unchanged. (Before
            // 2026-09-30 everything but index.html was `immutable`, and the app
            // window kept an old codeblocks.js for a year after an update.)
            let etag = format!("\"{}\"", hex::encode(&file.metadata.sha256_hash()[..16]));
            let cache = if content_hashed(path) {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            let fresh = req
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
            let mut resp = if fresh {
                StatusCode::NOT_MODIFIED.into_response()
            } else {
                (
                    [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                    file.data.into_owned(),
                )
                    .into_response()
            };
            let h = resp.headers_mut();
            h.insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static(cache),
            );
            if let Ok(v) = header::HeaderValue::from_str(&etag) {
                h.insert(header::ETAG, v);
            }
            if !csp.is_empty() {
                h.insert(
                    header::CONTENT_SECURITY_POLICY,
                    header::HeaderValue::from_static(csp),
                );
            }
            resp
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "lmgw-ui bundle missing: run `trunk build` in crates/lmgw-ui",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::content_hashed;

    #[test]
    fn only_trunk_hashed_names_count_as_content_hashed() {
        assert!(content_hashed("app-4dd5e9466ebb44f4.css"));
        assert!(content_hashed("lmgw-ui-b5b9f9525c7eb1fc_bg.wasm"));
        assert!(content_hashed("lmgw-ui-b5b9f9525c7eb1fc.js"));
        assert!(content_hashed("codeblocks-5affbd93e6a94087.js"));
        assert!(!content_hashed("index.html"));
        assert!(!content_hashed("codeblocks.js"));
        assert!(!content_hashed("vendor/mermaid/mermaid.min.js"));
        assert!(!content_hashed("vendor/highlight.js"));
        assert!(!content_hashed("fonts/NotoSans-Regular.woff2"));
        // Sixteen characters but not hex, and hex of the wrong length.
        assert!(!content_hashed("font-awesome-regularxx.css"));
        assert!(!content_hashed("x-4dd5e9466ebb44f.css"));
    }
}
