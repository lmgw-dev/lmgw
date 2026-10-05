//! Cache headers of the dashboard bundle (2026-09-30): only files whose name
//! carries Trunk's content hash are cached forever; everything else — index.html,
//! the copied `vendor/` libraries — is revalidated against its ETag, so an
//! update reaches the app window instead of a year-old cached copy.

use lmgw_core::state::AppState;

use crate::common::serve;

#[tokio::test]
async fn hashed_assets_are_immutable_and_the_rest_revalidates_by_etag() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state).await;
    let c = base.client();

    let index = c.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(index.headers()["cache-control"], "no-cache");
    assert!(index.headers().contains_key("etag"));
    let html = index.text().await.unwrap();

    // The chat glue module is loaded under its hashed name.
    let js = html
        .split('"')
        .find(|s| s.starts_with("/codeblocks-") && s.ends_with(".js"))
        .unwrap_or_else(|| panic!("no hashed codeblocks script in index.html:\n{html}"))
        .to_string();
    let r = c.get(format!("{base}{js}")).send().await.unwrap();
    assert_eq!(r.status(), 200, "{js}");
    assert_eq!(
        r.headers()["cache-control"],
        "public, max-age=31536000, immutable"
    );

    // An unhashed vendor file: no-cache + ETag, and a matching If-None-Match is a 304.
    let r = c
        .get(format!("{base}/vendor/highlight.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "no-cache");
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
    let r = c
        .get(format!("{base}/vendor/highlight.js"))
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 304);
    assert_eq!(r.headers()["etag"].to_str().unwrap(), etag);
    assert!(r.bytes().await.unwrap().is_empty());
    // A stale tag gets the bytes again.
    let r = c
        .get(format!("{base}/vendor/highlight.js"))
        .header("if-none-match", "\"stale\"")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // The old unhashed name is gone, so a year-old cached copy is never asked for.
    let r = c.get(format!("{base}/codeblocks.js")).send().await.unwrap();
    assert_eq!(r.status(), 404);
}

/// The voice worklets (chat-voice §11.3) are loaded by URL at runtime
/// (`audioWorklet.addModule("/voice/…")`), so Trunk copies them unhashed: they
/// must be in the bundle, served as JavaScript (a worklet module with another
/// type is refused), and revalidated like the vendor files.
#[tokio::test]
async fn the_voice_worklets_are_served_as_javascript_and_revalidate() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state).await;
    let c = base.client();
    for (file, processor) in [
        (
            "/voice/capture-worklet.js",
            "registerProcessor(\"lmgw-capture\"",
        ),
        (
            "/voice/player-worklet.js",
            "registerProcessor(\"lmgw-player\"",
        ),
    ] {
        let r = c.get(format!("{base}{file}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{file} is not in the bundle");
        let ct = r.headers()["content-type"].to_str().unwrap().to_string();
        assert!(ct.contains("javascript"), "{file}: {ct}");
        assert_eq!(r.headers()["cache-control"], "no-cache", "{file}");
        let body = r.text().await.unwrap();
        assert!(body.contains(processor), "{file} registers {processor}");
    }
}

/// The realtime panel's visualisation (chat-voice §10, WP9): the three
/// variants and their shared engine are ES modules imported by URL at
/// runtime (`/voice/viz/<variant>.js`), so they are copied unhashed beside
/// the worklets, served as JavaScript and revalidated; the `import()` the
/// page calls them with is a wasm-bindgen snippet, which keeps its name
/// across builds and so must revalidate too.
#[tokio::test]
async fn the_voice_visualisation_modules_are_served_as_javascript() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state).await;
    let c = base.client();
    for (file, export) in [
        ("/voice/viz/engine.js", "export function run("),
        ("/voice/viz/ribbon.js", "export function mount("),
        ("/voice/viz/orb.js", "export function mount("),
        ("/voice/viz/ring.js", "export function mount("),
    ] {
        let r = c.get(format!("{base}{file}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{file} is not in the bundle");
        let ct = r.headers()["content-type"].to_str().unwrap().to_string();
        assert!(ct.contains("javascript"), "{file}: {ct}");
        assert_eq!(r.headers()["cache-control"], "no-cache", "{file}");
        let body = r.text().await.unwrap();
        assert!(body.contains(export), "{file} has {export}");
    }
    // The snippet, by the path the app's own module imports it from.
    let html = c
        .get(format!("{base}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let app = html
        .split(['"', '\''])
        .find(|s| s.starts_with("/lmgw-ui-") && s.ends_with(".js"))
        .unwrap_or_else(|| panic!("no app module in index.html:\n{html}"))
        .to_string();
    let glue = c
        .get(format!("{base}{app}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let snippet = glue
        .split(['"', '\''])
        .find(|s| s.starts_with("./snippets/") && s.ends_with(".js"))
        .unwrap_or_else(|| panic!("{app} imports no snippet"))
        .trim_start_matches('.')
        .to_string();
    let r = c.get(format!("{base}{snippet}")).send().await.unwrap();
    assert_eq!(r.status(), 200, "{snippet}");
    let ct = r.headers()["content-type"].to_str().unwrap().to_string();
    assert!(ct.contains("javascript"), "{snippet}: {ct}");
    assert_eq!(r.headers()["cache-control"], "no-cache", "{snippet}");
    assert!(r.text().await.unwrap().contains("import(url)"), "{snippet}");
}
