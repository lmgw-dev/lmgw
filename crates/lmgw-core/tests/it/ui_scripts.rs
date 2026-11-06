//! The dashboard's page holds no inline script (chat-plane design, "WP3a:
//! the transport seam"): every script is a same-origin file, so the page
//! loads under `script-src 'self' 'wasm-unsafe-eval'`, the policy of an
//! extension page that will host the Chat. Trunk's own inline loader is
//! replaced by `<meta name="lmgw-wasm">` and `start.js` (Trunk.toml's
//! `pattern_script`). `scripts/csp-check.sh` loads the Chat in a browser
//! under that policy; this checks the bundle the gateway serves.

use lmgw_core::state::AppState;

use crate::common::serve;

/// Set by ci/check.sh, which builds the bundle before the suite: a missing
/// bundle fails the test instead of skipping it.
const BUNDLE_REQUIRED: &str = "LMGW_UI_BUNDLE_REQUIRED";

/// Every `<tag …>` opening tag named `tag` in `html`, as written.
fn tags<'a>(html: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}");
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i..];
        let end = after.find('>').map_or(after.len(), |e| e + 1);
        let t = &after[..end];
        // `<scripts>` is not `<script>`.
        if t[open.len()..].starts_with([' ', '>', '\n', '\t']) {
            out.push(t);
        }
        rest = &after[end..];
    }
    out
}

/// `html` without its `<!-- … -->` comments.
fn without_comments(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        rest = rest[i..].find("-->").map_or("", |e| &rest[i + e + 3..]);
    }
    out.push_str(rest);
    out
}

/// An attribute's value in an opening tag.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let i = tag.find(&key)? + key.len();
    tag[i..].split('"').next()
}

#[tokio::test]
async fn the_page_runs_no_inline_script_and_loads_every_script_from_its_origin() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state).await;
    let c = base.client();
    let html = c
        .get(format!("{base}/chat"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    if html.contains("lmgw-ui bundle missing") {
        // ci/check.sh builds the bundle before the suite and says so here: a
        // bundle missing under the gate is a failure, not a pass with nothing
        // checked. Elsewhere (a plain `cargo test`, GitHub's fmt-and-tests
        // job, which runs no trunk) the test says it skipped.
        assert!(
            std::env::var_os(BUNDLE_REQUIRED).is_none(),
            "no lmgw-ui bundle although {BUNDLE_REQUIRED} is set (ci/check.sh builds it with \
             trunk before the suite): the served page's scripts went unchecked"
        );
        eprintln!(
            "SKIP the_page_runs_no_inline_script…: no lmgw-ui bundle (trunk build in \
             crates/lmgw-ui); set {BUNDLE_REQUIRED}=1 to fail instead"
        );
        return;
    }
    // What the comments say about tags is not markup.
    let html = without_comments(&html);
    let scripts = tags(&html, "script");
    assert!(scripts.len() >= 4, "{scripts:?}");
    for s in &scripts {
        let src = attr(s, "src").unwrap_or_else(|| panic!("an inline script: {s}"));
        assert!(
            src.starts_with('/') && !src.starts_with("//"),
            "a script from elsewhere: {s}"
        );
        let r = c.get(format!("{base}{src}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{src}");
    }
    // No inline handler on any element.
    for t in html.split('<').skip(1) {
        let tag = t.split('>').next().unwrap_or_default();
        if tag.starts_with('!') {
            continue;
        }
        for a in tag.split_whitespace().skip(1) {
            assert!(
                !a.to_ascii_lowercase().starts_with("on"),
                "an inline handler: <{tag}>"
            );
        }
    }
    // The loader: the module file, and the build's names it reads.
    assert!(
        scripts
            .iter()
            .any(|s| attr(s, "src").is_some_and(|v| v.starts_with("/start-"))
                && attr(s, "type") == Some("module")),
        "no start.js module: {scripts:?}"
    );
    let meta = tags(&html, "meta")
        .into_iter()
        .find(|m| attr(m, "name") == Some("lmgw-wasm"))
        .unwrap_or_else(|| panic!("no <meta name=\"lmgw-wasm\">:\n{html}"));
    for (key, ext) in [("data-js", ".js"), ("data-wasm", "_bg.wasm")] {
        let path = attr(meta, key).unwrap_or_else(|| panic!("{key}: {meta}"));
        assert!(
            path.starts_with("/lmgw-ui-") && path.ends_with(ext),
            "{meta}"
        );
        let r = c.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path}");
    }
}

#[test]
fn the_tag_reader_finds_inline_and_file_scripts() {
    let html =
        r#"<head><script src="/a.js" type="module"></script><script>x()</script><scripts></head>"#;
    let s = tags(html, "script");
    assert_eq!(s.len(), 2);
    assert_eq!(attr(s[0], "src"), Some("/a.js"));
    assert_eq!(attr(s[1], "src"), None);
    assert_eq!(
        without_comments("a<!-- <script>x</script> -->b<!-- open"),
        "ab"
    );
}
