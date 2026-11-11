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
    assert!(m.nsis.is_none(), "a release without a Windows installer");
    // Without one, Windows has nothing to install; Linux has the RPM.
    assert_eq!(m.installer().is_some(), !cfg!(windows));
    // Written back without the key, as an older reader expects it.
    let back = serde_json::to_value(&m).unwrap();
    assert!(back.get("nsis").is_none(), "{back}");
}

#[test]
fn the_windows_installer_is_its_own_entry() {
    let json = r#"{
        "version": "0.1.42",
        "rpm": { "file": "lmgw-0.1.42-1.x86_64.rpm",
                 "url": "https://example/lmgw-0.1.42-1.x86_64.rpm" },
        "nsis": { "file": "lmgw_0.1.42_x64-setup.exe",
                  "url": "https://example/lmgw_0.1.42_x64-setup.exe",
                  "sha256": "ab" }
    }"#;
    let m: ReleaseManifest = serde_json::from_str(json).unwrap();
    let nsis = m.nsis.as_ref().unwrap();
    assert_eq!(nsis.file, "lmgw_0.1.42_x64-setup.exe");
    assert_eq!(nsis.sha256, "ab");
    let platform = m.installer().unwrap();
    let want = if cfg!(windows) {
        &nsis.file
    } else {
        &m.rpm.file
    };
    assert_eq!(&platform.file, want);
}
