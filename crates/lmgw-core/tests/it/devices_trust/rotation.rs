//! Rotation and the store's items (§5.5, D2): a new wrapping key is added
//! beside the old items and the others are deleted only after the key file
//! is written, a rotation that fails before that changes nothing, and the
//! `tls/` directory is refused when others could replace its files.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use lmgw_api_types::device_link::{DeviceCaErrorKind, DeviceCaState};
use lmgw_core::devices::tls::{
    KeyMode, MemoryWrapStore, Trust, TrustError, TrustFiles, WrapKey, WrapKeyStore,
};

use super::trust_with;

fn chmod(p: &std::path::Path, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[tokio::test]
async fn a_rotation_that_fails_before_the_key_is_written_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let files = TrustFiles::in_data_dir(dir.path());
    let store = Arc::new(MemoryWrapStore::default());
    let t = trust_with(&store);
    let ca = t.open(dir.path(), true).await.unwrap();
    t.leaves
        .leaf(&ca, "127.0.0.1".parse().unwrap(), 30, chrono::Utc::now())
        .unwrap();
    let key_before = std::fs::read(&files.key).unwrap();
    let unchanged = |t: &Trust| {
        assert!(Arc::ptr_eq(&t.ca().unwrap(), &ca), "the CA in force stays");
        assert_eq!(t.status(dir.path()).ca.state, DeviceCaState::Ready);
        assert_eq!(t.leaves.leaves().len(), 1, "the leaves stay");
        assert_eq!(std::fs::read(&files.key).unwrap(), key_before);
    };

    // The prompt declined, and the store failing: before anything is
    // written.
    store.set_declined(true);
    let e = t.replace(dir.path()).await.unwrap_err();
    assert!(matches!(e, TrustError::Locked(_)), "{e}");
    unchanged(&t);
    store.set_declined(false);
    store.set_failure(Some("ksecretd crashed"));
    assert!(matches!(
        t.replace(dir.path()).await.unwrap_err(),
        TrustError::SecretService(_)
    ));
    unchanged(&t);
    store.set_failure(None);
    assert_eq!(store.count(dir.path()), 1);

    // The new item added, then the key file not written (a crash, or a
    // directory that refuses it): the old file still opens with its item.
    chmod(&files.dir, 0o500);
    let e = t.replace(dir.path()).await.unwrap_err();
    chmod(&files.dir, 0o700);
    assert!(e.to_string().contains("tls/ca.key"), "{e}");
    unchanged(&t);
    assert_eq!(store.count(dir.path()), 2, "the new item, beside the old");
    let restarted = trust_with(&store).open(dir.path(), false).await.unwrap();
    assert_eq!(restarted.fp(), ca.fp());

    // The next rotation that writes prunes every item but its own.
    let new = t.replace(dir.path()).await.unwrap();
    assert_ne!(new.fp(), ca.fp());
    assert!(t.leaves.leaves().is_empty());
    assert_eq!(store.count(dir.path()), 1);
    let back = trust_with(&store).open(dir.path(), false).await.unwrap();
    assert_eq!(back.fp(), new.fp());
}

#[tokio::test]
async fn items_that_open_nothing_never_stop_the_ca_and_go_at_the_next_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryWrapStore::default());
    // A headless start: plain.
    store.set_unavailable(true);
    let ca = trust_with(&store).open(dir.path(), true).await.unwrap();
    store.set_unavailable(false);
    // A stray item for this data directory (a provider that ignored
    // `replace`, a crash between add and write).
    store
        .add(dir.path(), &WrapKey::generate().unwrap())
        .await
        .unwrap();
    // The next start wraps the plain key, and keeps only its own item.
    let wrapped = trust_with(&store).open(dir.path(), false).await.unwrap();
    assert_eq!(
        (wrapped.fp(), wrapped.key_mode()),
        (ca.fp(), KeyMode::Wrapped)
    );
    assert_eq!(store.count(dir.path()), 1);
    // A stray item beside a wrapped key: the key file opens with its own.
    store
        .add(dir.path(), &WrapKey::generate().unwrap())
        .await
        .unwrap();
    let again = trust_with(&store).open(dir.path(), false).await.unwrap();
    assert_eq!(again.fp(), ca.fp());
}

#[tokio::test]
async fn a_tls_directory_others_could_write_to_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let files = TrustFiles::in_data_dir(dir.path());
    Trust::default().open(dir.path(), true).await.unwrap();
    for mode in [0o770, 0o702, 0o777] {
        chmod(&files.dir, mode);
        let t = Trust::default();
        let st = t.status(dir.path()).ca;
        assert_eq!(st.state, DeviceCaState::Error);
        let e = st.error.unwrap();
        assert_eq!(e.kind, DeviceCaErrorKind::BadMode, "{}", e.message);
        assert!(e.message.starts_with("tls/ has mode"), "{}", e.message);
        assert!(matches!(
            t.open(dir.path(), true).await.unwrap_err(),
            TrustError::BadMode(_)
        ));
        // A rotation does not write into it either.
        assert!(matches!(
            t.replace(dir.path()).await.unwrap_err(),
            TrustError::BadMode(_)
        ));
    }
    // Readable by others is no refusal for the directory (the key file is
    // 0600 and the certificate public); writable is.
    chmod(&files.dir, 0o755);
    Trust::default().open(dir.path(), false).await.unwrap();
    chmod(&files.dir, 0o700);

    // A symbolic link in its place is refused.
    let elsewhere = dir.path().join("elsewhere");
    std::fs::rename(&files.dir, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &files.dir).unwrap();
    let e = Trust::default().open(dir.path(), false).await.unwrap_err();
    assert!(
        matches!(&e, TrustError::BadMode(m) if m.contains("not a directory of its own")),
        "{e}"
    );
}
