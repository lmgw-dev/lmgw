//! The device CA's wrapping keys in Windows Credential Manager
//! (`devices::tls::CredentialManagerStore`, Windows design §4.4, WP6): add
//! beside, find by data directory exactly (whatever its spelling: case, the
//! `\\?\` prefix), prune to the one kept; a data dir whose path is longer
//! than a credential's comment may be (256 UTF-16 units) works too.
//!
//! A logon without credentials (an SSH logon by key, as on a remote Windows check machine) has no store: the test then checks that
//! this is said in its own words, as `Unavailable`. With
//! `LMGW_CREDMAN_REQUIRED=1` that answer fails the test instead, for a run in
//! a logon that has a store (the desktop session, or a task run as SYSTEM).

use lmgw_core::devices::tls::{CredentialManagerStore, StoreError, WrapKey, WrapKeyStore};

#[tokio::test]
async fn the_credential_manager_adds_beside_finds_by_directory_and_prunes_or_says_why() {
    let store = CredentialManagerStore;
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join("data");
    std::fs::create_dir(&dir).unwrap();
    // A directory whose name starts with the first's: never its keys.
    let longer = base.path().join("data-longer");
    // The same directory spelt another way: in upper case, and verbatim.
    let upper = std::path::PathBuf::from(dir.to_string_lossy().to_uppercase());
    let canonical = std::fs::canonicalize(&dir).unwrap();
    assert!(canonical.to_string_lossy().starts_with(r"\\?\"));
    // Past a comment's 256 units; not created, so it is named as given.
    let long = base
        .path()
        .join("l".repeat(120))
        .join("m".repeat(120))
        .join("data");
    let (a, b) = (WrapKey::generate().unwrap(), WrapKey::generate().unwrap());
    match store.add(&dir, &a).await {
        Err(StoreError::Unavailable(why)) => {
            assert!(
                std::env::var("LMGW_CREDMAN_REQUIRED").as_deref() != Ok("1"),
                "Credential Manager was required and answered: {why}"
            );
            assert!(why.contains("ERROR_NO_SUCH_LOGON_SESSION"), "{why}");
            assert!(matches!(
                store.get(&dir).await,
                Err(StoreError::Unavailable(_)) | Ok(_)
            ));
            println!("credential manager: no store in this logon session: {why}");
            return;
        }
        other => other.unwrap(),
    }
    store.add(&dir, &b).await.unwrap();
    store.add(&longer, &a).await.unwrap();
    assert_eq!(store.get(&dir).await.unwrap().len(), 2);
    assert_eq!(store.get(&upper).await.unwrap().len(), 2);
    assert_eq!(store.get(&canonical).await.unwrap().len(), 2);
    assert_eq!(store.prune(&upper, &b).await.unwrap(), 1);
    let left = store.get(&dir).await.unwrap();
    assert!(left.len() == 1 && left[0].same(&b));
    // The other directory's key is still there; then both are cleared.
    assert_eq!(store.get(&longer).await.unwrap().len(), 1);
    let none = WrapKey::generate().unwrap();
    assert_eq!(store.prune(&dir, &none).await.unwrap(), 1);
    assert_eq!(store.prune(&longer, &none).await.unwrap(), 1);
    assert!(store.get(&dir).await.unwrap().is_empty());

    assert!(long.as_os_str().len() > 256);
    store.add(&long, &a).await.unwrap();
    assert_eq!(store.get(&long).await.unwrap().len(), 1);
    assert_eq!(store.prune(&long, &none).await.unwrap(), 1);
    println!("credential manager: stored, found under every spelling, pruned and cleared");
}
