//! The header table drift guards (api-docs design §7.1): every `x-lmgw-*`
//! literal in the source is in `LMGW_HEADERS`, both ways, and `/v1/models`'
//! `lmgw.headers` lists exactly the client-audience rows. WP2.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use lmgw_core::openapi::{headers_block, Audience, LMGW_HEADERS};
use lmgw_core::state::AppState;

use crate::common;

// ---------------------------------------------------------------------------
// 1. Every `"x-lmgw-*"` literal in `src/` is a table row, and vice versa.
// ---------------------------------------------------------------------------

#[test]
fn every_x_lmgw_literal_is_in_the_header_table() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    // Not the description itself (review R2 #4): `openapi/headers.rs` *is*
    // the table, so scanning it made every row find its own name and the
    // "no longer sent anywhere" direction below could never fail.
    let openapi = src.join("openapi");
    files.retain(|f| !f.starts_with(&openapi));
    assert!(
        files.len() > 50,
        "the source walk found only {} files, which means it stopped working",
        files.len()
    );

    let mut found: BTreeSet<String> = BTreeSet::new();
    for file in &files {
        literals(file, &mut found);
    }
    assert!(
        found.len() > 10,
        "the literal scan found only {found:?}, which means it stopped working"
    );

    let table: BTreeSet<String> = LMGW_HEADERS.iter().map(|h| h.name.to_string()).collect();

    let missing_from_table: Vec<_> = found.difference(&table).collect();
    assert!(
        missing_from_table.is_empty(),
        "these x-lmgw-* literals are in src/ but not in LMGW_HEADERS: {missing_from_table:#?}"
    );

    let missing_from_source: Vec<_> = table.difference(&found).collect();
    assert!(
        missing_from_source.is_empty(),
        "these LMGW_HEADERS rows name no x-lmgw-* literal anywhere in src/: \
         {missing_from_source:#?}"
    );
}

/// Every `.rs` file under `dir`, except `tests.rs` (a test module's own
/// scaffolding, not the gateway's surface — same exclusion `route_walk.rs`
/// makes).
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && path.file_name().is_some_and(|n| n != "tests.rs")
        {
            out.push(path);
        }
    }
}

/// Every distinct `"x-lmgw-…"` string literal in `file`, outside its
/// `#[cfg(test)]` tail (a test's own fixtures, not the gateway's surface),
/// lowercased. Case-insensitive (review R2 #4): an HTTP header name is, and a
/// `.header("X-Lmgw-Run", ..)` names the same header the table's
/// `x-lmgw-run` row documents.
fn literals(file: &Path, out: &mut BTreeSet<String>) {
    let text = std::fs::read_to_string(file).unwrap();
    let text = text.split("#[cfg(test)]").next().unwrap();
    // ASCII lowercasing keeps every byte offset where it was.
    let lower = text.to_ascii_lowercase();

    let mut i = 0;
    while let Some(at) = lower[i..].find("\"x-lmgw-") {
        let start = i + at + 1; // just past the opening quote
        let Some(end_rel) = lower[start..].find('"') else {
            break;
        };
        let end = start + end_rel;
        let literal = &lower[start..end];
        // Only a full `x-lmgw-<name>` token counts: `image_lab.rs`'s
        // `starts_with("x-lmgw-")` prefix check is not a header name (it has
        // nothing after the trailing hyphen).
        if literal.len() > "x-lmgw-".len()
            && literal[7..]
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            out.insert(literal.to_string());
        }
        i = end + 1;
    }
}

#[test]
fn the_literal_scan_is_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("probe.rs");
    std::fs::write(
        &file,
        "fn f() { h.header(\"X-Lmgw-Probe\", v); g(\"x-lmgw-other\"); }\n\
         #[cfg(test)]\nmod tests { const T: &str = \"x-lmgw-test-only\"; }\n",
    )
    .unwrap();
    let mut found = BTreeSet::new();
    literals(&file, &mut found);
    assert_eq!(
        found,
        BTreeSet::from(["x-lmgw-probe".to_string(), "x-lmgw-other".to_string()])
    );
}

// ---------------------------------------------------------------------------
// 2. `/v1/models`' `lmgw.headers` is exactly the client-audience rows.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_models_block_lists_the_client_headers() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state).await;

    let resp = gw
        .client()
        .get(format!("{gw}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    let served: BTreeSet<String> = body["lmgw"]["headers"]
        .as_object()
        .unwrap_or_else(|| panic!("no lmgw.headers object: {body}"))
        .keys()
        .cloned()
        .collect();

    let expected: BTreeSet<String> = headers_block().keys().map(|s| s.to_string()).collect();
    assert_eq!(served, expected, "served vs. Audience::Client rows");

    // And nothing non-client leaked in.
    for h in LMGW_HEADERS {
        if h.audience != Audience::Client {
            assert!(
                !served.contains(h.name),
                "{} is {:?}, not Client, and must not be in /v1/models",
                h.name,
                h.audience
            );
        }
    }
}
