//! The admin document a test run shares between its processes
//! (`openapi::doc_cache`, debug builds only): the cached copy is the
//! document this build makes, a file that does not parse fails, and a file
//! from another run or binary is built again.

use lmgw_core::openapi::{self, doc_cache};
use serde_json::json;

/// The document `admin_doc` gave this test is the one this build makes,
/// and so is the file a run shares: built here for real, never read from
/// the cache. Without the cache (plain `cargo test`), `admin_doc` built it
/// too, and this says so.
#[test]
fn the_cached_document_is_the_one_this_build_makes() {
    let served = openapi::admin_doc();
    let built = openapi::build_admin_doc();
    assert!(
        *served == built,
        "admin_doc() differs from the document this build makes"
    );
    match std::env::var_os(doc_cache::ENV) {
        Some(path) => {
            let path = std::path::Path::new(&path);
            let cached = doc_cache::read(path, &doc_cache::stamp()).unwrap_or_else(|| {
                panic!(
                    "{} holds no document of this run after admin_doc() (or another nextest \
                     run in this target dir wrote it since)",
                    path.display()
                )
            });
            assert!(
                cached == built,
                "{} differs from the document this build makes",
                path.display()
            );
        }
        None => eprintln!(
            "{} is not set (no nextest setup script): admin_doc() built the document itself",
            doc_cache::ENV
        ),
    }
}

#[test]
#[should_panic(expected = "does not parse")]
fn a_cache_file_that_does_not_parse_is_a_hard_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.json");
    std::fs::write(&path, b"{\"stamp\": \"run exe\", \"doc\": {").unwrap();
    doc_cache::load_or_build(&path, "run exe", || panic!("built instead of failing"));
}

#[test]
fn a_cache_from_another_run_is_built_again_and_then_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.json");
    std::fs::write(
        &path,
        json!({"stamp": "old exe", "doc": {"old": true}}).to_string(),
    )
    .unwrap();

    let doc = doc_cache::load_or_build(&path, "new exe", || json!({"new": true}));
    assert_eq!(doc, json!({"new": true}));
    assert_eq!(
        doc_cache::read(&path, "new exe"),
        Some(json!({"new": true}))
    );
    assert_eq!(doc_cache::read(&path, "old exe"), None);

    let again = doc_cache::load_or_build(&path, "new exe", || panic!("built twice"));
    assert_eq!(again, json!({"new": true}));
}

#[test]
fn no_file_is_a_build() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.json");
    assert_eq!(doc_cache::read(&path, "run exe"), None);
    assert_eq!(
        doc_cache::load_or_build(&path, "run exe", || json!([1])),
        json!([1])
    );
}
