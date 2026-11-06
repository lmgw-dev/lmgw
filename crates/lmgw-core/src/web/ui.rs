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
const API_PREFIXES: [&str; 9] = [
    "api/",
    "v1/",
    "chat/api/",
    "audio-lab/api/",
    "image-lab/api/",
    "decision-lab/api/",
    "voice-lab/api/",
    "labs/api/",
    "transfers/api/",
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
        // The one exemption from the router-wide nosniff + sandbox default
        // (machine-check design D90): every answer of this router is the
        // dashboard's own — the page, its assets, the client-side routes'
        // fallback, the `/ui` redirect, and this router's 404, 503 and
        // refusals — and keeps the policy `respond` gives it. `layer`, not
        // `route_layer`, so the method router's own 405 is marked too.
        .layer(axum::middleware::map_response(
            super::stored::dashboard_owned,
        ))
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
///
/// One `UiDist::get` per file: in a debug build it reads the file from disk
/// and hashes it (rust-embed's ETag), which for the debug wasm is most of the
/// response time, so the file found here is the one served.
async fn asset(Path(path): Path<String>, headers: HeaderMap) -> Response {
    if let Some(file) = UiDist::get(&path) {
        respond(&path, file, &headers)
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

/// The dashboard's Permissions Policy, set on every HTML response beside the
/// CSP (chat-voice §13.1). The microphone is the dashboard's own: no frame of
/// another origin — an agent app's, the opaque HTML preview — can be handed
/// it, even by an `<iframe allow="microphone">` the UI should never write;
/// the camera and screen capture are nobody's. Same-origin frames still
/// inherit the microphone (`self`), which is why no same-origin frame may
/// show untrusted content.
const DASHBOARD_PERMISSIONS: &str = "microphone=(self), camera=(), display-capture=()";

/// Whether a bundle file's name carries Trunk's content hash
/// (`app-4dd5e9466ebb44f4.css`, `lmgw-ui-b5b9f9525c7eb1fc_bg.wasm`): a new
/// build gives changed bytes a new name, so such a file may be cached forever.
///
/// Trunk writes the hash as `{:x}` of a 64-bit seahash (its asset pipeline's
/// `"{stem}-{:x}.{ext}"`, its Rust pipeline's `"{name}-{hash:x}"`), so
/// without leading zeros: 16 lowercase hex digits in 15 builds of 16, 15 in
/// one of 16 (`lmgw-ui-719808ae117a2a9_bg.wasm`), fewer ever more rarely.
/// Down to [`TRUNK_HASH_MIN_DIGITS`] count; a shorter one (one name in about
/// a million) is served `no-cache`, which only costs a revalidation, while a
/// short hex word a copied file may end in (`-cafe`, `-b`) never counts.
fn content_hashed(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some((stem, _ext)) = name.rsplit_once('.') else {
        return false;
    };
    let stem = stem.strip_suffix("_bg").unwrap_or(stem);
    stem.rsplit_once('-').is_some_and(|(_, h)| {
        (TRUNK_HASH_MIN_DIGITS..=16).contains(&h.len())
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// The fewest hex digits [`content_hashed`] takes for Trunk's hash: a 64-bit
/// value has fewer than 12 only when its top five nibbles are zero, one
/// value in 16^5.
const TRUNK_HASH_MIN_DIGITS: usize = 12;

fn serve(path: &str, req: &HeaderMap) -> Response {
    match UiDist::get(path) {
        Some(file) => respond(path, file, req),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "lmgw-ui bundle missing: run `trunk build` in crates/lmgw-ui",
        )
            .into_response(),
    }
}

fn respond(path: &str, file: rust_embed::EmbeddedFile, req: &HeaderMap) -> Response {
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
        h.insert(
            header::HeaderName::from_static("permissions-policy"),
            header::HeaderValue::from_static(DASHBOARD_PERMISSIONS),
        );
    }
    resp
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
        // Sixteen characters but not hex.
        assert!(!content_hashed("font-awesome-regularxx.css"));
        // Trunk's `{:x}` drops leading zeros: a hash below 2^60 has 15
        // digits (a real build's wasm and glue), one below 2^48 twelve.
        assert!(content_hashed("lmgw-ui-719808ae117a2a9_bg.wasm"));
        assert!(content_hashed("lmgw-ui-719808ae117a2a9.js"));
        assert!(content_hashed("x-4dd5e9466ebb44f.css"));
        assert!(content_hashed("x-4dd5e9466ebb.css"));
        assert!(content_hashed(&format!(
            "x-{:x}.js",
            0x0000_ff00_dead_beef_u64
        )));
        // Shorter is a word, not a hash; longer is not a 64-bit value; upper
        // case is not Trunk's.
        assert!(!content_hashed("x-4dd5e9466eb.css"));
        assert!(!content_hashed("vendor/katex-cafe.css"));
        assert!(!content_hashed("x-4dd5e9466ebb44f4a.css"));
        assert!(!content_hashed("x-4DD5E9466EBB44F4.css"));
    }
}
