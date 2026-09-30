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
