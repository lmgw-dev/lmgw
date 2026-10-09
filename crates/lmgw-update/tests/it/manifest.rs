//! `latest.json` as CI publishes it.

use lmgw_update::ReleaseManifest;

#[test]
fn manifest_parses_with_optional_fields() {
    let json = r#"{
        "version": "0.1.42",
        "rpm": { "file": "lmgw-0.1.42-1.x86_64.rpm",
                 "url": "https://example/lmgw-0.1.42-1.x86_64.rpm" }
    }"#;
    let m: ReleaseManifest = serde_json::from_str(json).unwrap();
    assert_eq!(m.version, "0.1.42");
    assert_eq!(m.rpm.sha256, "");
    assert!(m.notes.is_empty());
}
