//! The CA (§2.1, D2): created at the first open, its files and their modes,
//! the wrapping and its modes, `fp`, and the refusals that leave it down.

use std::sync::Arc;

use lmgw_api_types::device_link::{DeviceCaErrorKind, DeviceCaKeyMode, DeviceCaState};
use lmgw_core::devices::tls::{
    self, KeyMode, MemoryWrapStore, Trust, TrustError, TrustFiles, PLAIN_LABEL, WRAPPED_LABEL,
};
use lmgw_core::state::AppState;
use sha2::{Digest, Sha256};

use super::{mode_of, plain_ca, trust_with};

fn label(p: &std::path::Path) -> String {
    pem::parse(std::fs::read(p).unwrap())
        .unwrap()
        .tag()
        .to_string()
}

/// `fp` recomputed from `ca.crt` as the owner would: the certificate's DER
/// SubjectPublicKeyInfo, hashed.
fn fp_from_file(files: &TrustFiles) -> String {
    let der = pem::parse(std::fs::read(&files.cert).unwrap())
        .unwrap()
        .into_contents();
    let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
    format!(
        "{}{}",
        lmgw_api_types::device_link::FP_PREFIX,
        hex::encode(Sha256::digest(cert.public_key().raw))
    )
}

#[tokio::test]
async fn the_first_open_makes_a_wrapped_ca_in_its_stated_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryWrapStore::default());
    let trust = trust_with(&store);
    let files = TrustFiles::in_data_dir(dir.path());

    // Nothing exists until the listener's first start asks for one.
    assert_eq!(trust.status(dir.path()).ca.state, DeviceCaState::Missing);
    assert_eq!(
        trust.open(dir.path(), false).await.unwrap_err(),
        TrustError::Missing
    );
    assert!(!files.dir.exists());

    let ca = trust.open(dir.path(), true).await.unwrap();
    assert_eq!(mode_of(&files.dir), 0o700);
    assert_eq!(mode_of(&files.key), 0o600);
    assert_eq!(mode_of(&files.cert), 0o644);
    assert_eq!(label(&files.key), WRAPPED_LABEL);
    assert_eq!(label(&files.cert), "CERTIFICATE");
    assert_eq!(store.count(dir.path()), 1);
    assert_eq!(ca.key_mode(), KeyMode::Wrapped);
    assert_eq!(ca.fp(), fp_from_file(&files));
    assert_eq!(
        ca.certificate_pem().as_bytes(),
        std::fs::read(&files.cert).unwrap()
    );

    let st = trust.status(dir.path()).ca;
    assert_eq!(st.state, DeviceCaState::Ready);
    assert_eq!(st.key_mode, Some(DeviceCaKeyMode::Wrapped));
    assert_eq!(st.fp.as_deref(), Some(ca.fp()));
    assert_eq!(st.error, None);
    assert_eq!(st.key_file, "tls/ca.key");
    assert_eq!(st.cert_file, "tls/ca.crt");

    // Loaded once: the next open is the same CA, and the store is not asked.
    let calls = store.calls();
    let again = trust.open(dir.path(), true).await.unwrap();
    assert!(Arc::ptr_eq(&ca, &again));
    assert_eq!(store.calls(), calls);

    // A restart opens the same CA with the stored wrapping key.
    let restarted = trust_with(&store).open(dir.path(), false).await.unwrap();
    assert_eq!(restarted.fp(), ca.fp());
    assert_eq!(restarted.key_mode(), KeyMode::Wrapped);
}

#[tokio::test]
async fn the_ca_certificate_is_as_the_design_states() {
    let dir = tempfile::tempdir().unwrap();
    let (_, ca) = plain_ca(dir.path()).await;
    let der = ca.certificate().to_vec();
    let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();

    let hex = ca.fp().trim_start_matches("sha256:");
    assert_eq!(hex.len(), 64);
    let cn: Vec<_> = cert
        .subject()
        .iter_common_name()
        .map(|a| a.as_str().unwrap().to_string())
        .collect();
    assert_eq!(cn, [format!("lmgw devices {}", &hex[..8])]);
    let o: Vec<_> = cert
        .subject()
        .iter_organization()
        .map(|a| a.as_str().unwrap().to_string())
        .collect();
    assert_eq!(o, ["lmgw"]);
    assert_eq!(cert.subject(), cert.issuer(), "self-signed");
    cert.verify_signature(None).expect("its own key signed it");

    let bc = cert.basic_constraints().unwrap().unwrap();
    assert!(bc.critical);
    assert!(bc.value.ca);
    assert_eq!(bc.value.path_len_constraint, Some(0));
    let ku = cert.key_usage().unwrap().unwrap().value;
    assert!(ku.key_cert_sign() && ku.crl_sign());
    assert!(!ku.digital_signature() && !ku.key_encipherment());
    assert!(cert.extended_key_usage().unwrap().is_none());

    // notBefore its creation day; notAfter 9999-12-31T23:59:59Z, written as
    // a GeneralizedTime (§16: rcgen settles it).
    let today = chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    assert_eq!(cert.validity().not_before.timestamp(), today.timestamp());
    assert_eq!(cert.validity().not_after.timestamp(), tls::NO_EXPIRY);
    let generalized = [&[0x18u8, 0x0f][..], b"99991231235959Z"].concat();
    assert!(
        der.windows(generalized.len()).any(|w| w == generalized),
        "notAfter is a GeneralizedTime"
    );
    assert_eq!(
        ca.facts().created.timestamp(),
        today.timestamp(),
        "the status's creation day"
    );

    // A random positive serial of at most 127 bits.
    let serial = cert.raw_serial();
    assert!(serial.len() <= 16 && serial[0] < 0x80, "{serial:02x?}");
    // EC P-256.
    assert_eq!(
        cert.public_key().algorithm.algorithm.to_id_string(),
        "1.2.840.10045.2.1"
    );
    assert_eq!(
        cert.public_key()
            .algorithm
            .parameters
            .as_ref()
            .unwrap()
            .as_oid()
            .unwrap()
            .to_id_string(),
        "1.2.840.10045.3.1.7"
    );
    // Two CAs never share a key or a serial.
    let other = tempfile::tempdir().unwrap();
    let (_, ca2) = plain_ca(other.path()).await;
    assert_ne!(ca2.fp(), ca.fp());
    let (_, cert2) = x509_parser::parse_x509_certificate(ca2.certificate()).unwrap();
    assert_ne!(cert2.raw_serial(), serial);
}

#[tokio::test]
async fn a_headless_start_keeps_the_key_plain_and_the_next_start_wraps_it() {
    let dir = tempfile::tempdir().unwrap();
    let files = TrustFiles::in_data_dir(dir.path());
    let store = Arc::new(MemoryWrapStore::default());
    store.set_unavailable(true);
    let ca = trust_with(&store).open(dir.path(), true).await.unwrap();
    assert_eq!(ca.key_mode(), KeyMode::Plain);
    assert_eq!(label(&files.key), PLAIN_LABEL);
    assert_eq!(mode_of(&files.key), 0o600);
    assert_eq!(store.count(dir.path()), 0);
    let plain = std::fs::read(&files.key).unwrap();
    // Standard PKCS#8: what `openssl pkey` reads.
    let pkcs8 = pem::parse(&plain).unwrap().into_contents();
    rcgen::KeyPair::try_from(pkcs8.as_slice()).expect("a PKCS#8 key");

    // The next start reaches a Secret Service: it wraps the key and replaces
    // the plain copy.
    store.set_unavailable(false);
    let next = trust_with(&store);
    let peeked = next.status(dir.path()).ca;
    assert_eq!(peeked.state, DeviceCaState::Stored);
    assert_eq!(peeked.key_mode, Some(DeviceCaKeyMode::Plain));
    let wrapped = next.open(dir.path(), false).await.unwrap();
    assert_eq!(wrapped.key_mode(), KeyMode::Wrapped);
    assert_eq!(wrapped.fp(), ca.fp());
    assert_eq!(label(&files.key), WRAPPED_LABEL);
    assert_eq!(mode_of(&files.key), 0o600);
    assert_eq!(store.count(dir.path()), 1);
    let sealed = pem::parse(std::fs::read(&files.key).unwrap())
        .unwrap()
        .into_contents();
    assert!(
        !sealed
            .windows(16)
            .any(|w| pkcs8.windows(16).any(|p| p == w)),
        "no part of the plain key is left in the file"
    );
    assert_eq!(
        next.status(dir.path()).ca.key_mode,
        Some(DeviceCaKeyMode::Wrapped)
    );
    // And it stays wrapped from then on.
    let third = trust_with(&store).open(dir.path(), false).await.unwrap();
    assert_eq!(third.key_mode(), KeyMode::Wrapped);
}

#[tokio::test]
async fn a_wrapped_key_without_its_store_or_its_item_leaves_the_ca_down() {
    let dir = tempfile::tempdir().unwrap();
    let files = TrustFiles::in_data_dir(dir.path());
    let store = Arc::new(MemoryWrapStore::default());
    let ca = trust_with(&store).open(dir.path(), true).await.unwrap();
    let cert_before = std::fs::read(&files.cert).unwrap();

    let refused = |t: &Trust, kind: DeviceCaErrorKind| {
        let st = t.status(dir.path()).ca;
        assert_eq!(st.state, DeviceCaState::Error);
        let e = st.error.clone().unwrap();
        assert_eq!(e.kind, kind, "{}", e.message);
        // What the files still show is shown beside the reason.
        assert_eq!(st.fp.as_deref(), Some(ca.fp()));
        assert_eq!(st.key_mode, Some(DeviceCaKeyMode::Wrapped));
        e.message
    };

    // A headless start.
    store.set_unavailable(true);
    let t = trust_with(&store);
    let e = t.open(dir.path(), true).await.unwrap_err();
    assert!(matches!(e, TrustError::NoSecretService(_)), "{e}");
    let msg = refused(&t, DeviceCaErrorKind::NoSecretService);
    assert!(msg.contains("tls/ca.key"), "{msg}");
    assert!(!msg.contains(&dir.path().display().to_string()), "{msg}");
    store.set_unavailable(false);

    // The unlock prompt declined.
    store.set_declined(true);
    let t = trust_with(&store);
    assert!(matches!(
        t.open(dir.path(), true).await.unwrap_err(),
        TrustError::Locked(_)
    ));
    refused(&t, DeviceCaErrorKind::Locked);
    store.set_declined(false);

    // The service failing otherwise.
    store.set_failure(Some("ksecretd crashed"));
    let t = trust_with(&store);
    let e = t.open(dir.path(), true).await.unwrap_err();
    assert!(e.to_string().contains("ksecretd crashed"), "{e}");
    refused(&t, DeviceCaErrorKind::SecretService);
    store.set_failure(None);

    // The item gone.
    store.forget(dir.path());
    let t = trust_with(&store);
    assert!(matches!(
        t.open(dir.path(), true).await.unwrap_err(),
        TrustError::WrapKeyMissing(_)
    ));
    let msg = refused(&t, DeviceCaErrorKind::WrapKeyMissing);
    assert!(msg.contains("rotate the device CA"), "{msg}");

    // An open never replaces a CA that exists, `create` or not.
    assert_eq!(std::fs::read(&files.cert).unwrap(), cert_before);
    // A failure is retried at the next open: the owner rotates, and it
    // serves again.
    let rotated = t.replace(dir.path()).await.unwrap();
    assert_ne!(rotated.fp(), ca.fp());
    assert_eq!(t.status(dir.path()).ca.state, DeviceCaState::Ready);
}

#[tokio::test]
async fn a_declined_unlock_at_creation_writes_no_ca() {
    let dir = tempfile::tempdir().unwrap();
    let files = TrustFiles::in_data_dir(dir.path());
    let store = Arc::new(MemoryWrapStore::default());
    store.set_declined(true);
    let t = trust_with(&store);
    let e = t.open(dir.path(), true).await.unwrap_err();
    assert!(matches!(e, TrustError::Locked(_)), "{e}");
    assert!(e.to_string().contains("declined"), "{e}");
    assert!(!files.key.exists() && !files.cert.exists());
    assert_eq!(t.status(dir.path()).ca.state, DeviceCaState::Error);
    // Accepted the next time: made then.
    store.set_declined(false);
    let ca = t.open(dir.path(), true).await.unwrap();
    assert_eq!(ca.key_mode(), KeyMode::Wrapped);
}

#[tokio::test]
async fn a_key_file_others_can_reach_is_refused_by_name() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let files = TrustFiles::in_data_dir(dir.path());
    let (_, ca) = plain_ca(dir.path()).await;

    for mode in [0o640, 0o604, 0o660] {
        std::fs::set_permissions(&files.key, std::fs::Permissions::from_mode(mode)).unwrap();
        let t = Trust::default();
        // Seen before the listener starts, and refused when it does.
        let st = t.status(dir.path()).ca;
        assert_eq!(st.state, DeviceCaState::Error);
        let e = st.error.unwrap();
        assert_eq!(e.kind, DeviceCaErrorKind::BadMode);
        assert!(e.message.contains("tls/ca.key"), "{}", e.message);
        assert!(e.message.contains(&format!("{mode:04o}")), "{}", e.message);
        assert!(matches!(
            t.open(dir.path(), true).await.unwrap_err(),
            TrustError::BadMode(_)
        ));
    }
    std::fs::set_permissions(&files.key, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        Trust::default().open(dir.path(), false).await.unwrap().fp(),
        ca.fp()
    );

    // A symbolic link is refused too, whatever it points at.
    let real = dir.path().join("elsewhere.key");
    std::fs::rename(&files.key, &real).unwrap();
    std::os::unix::fs::symlink(&real, &files.key).unwrap();
    let e = Trust::default().open(dir.path(), false).await.unwrap_err();
    assert!(
        matches!(&e, TrustError::BadMode(m) if m.contains("symbolic link")),
        "{e}"
    );
}

#[tokio::test]
async fn half_a_ca_or_another_cas_key_is_refused() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let fa = TrustFiles::in_data_dir(a.path());
    let fb = TrustFiles::in_data_dir(b.path());
    plain_ca(a.path()).await;
    plain_ca(b.path()).await;

    // B's key beside A's certificate.
    std::fs::copy(&fb.key, &fa.key).unwrap();
    let e = Trust::default().open(a.path(), true).await.unwrap_err();
    assert!(
        matches!(&e, TrustError::Unreadable(m) if m.contains("does not belong")),
        "{e}"
    );

    // A certificate alone, then a key alone.
    std::fs::remove_file(&fa.key).unwrap();
    let t = Trust::default();
    let e = t.open(a.path(), true).await.unwrap_err();
    assert!(matches!(e, TrustError::Unreadable(_)), "{e}");
    assert_eq!(
        t.status(a.path()).ca.error.unwrap().kind,
        DeviceCaErrorKind::Unreadable
    );
    std::fs::remove_file(&fb.cert).unwrap();
    let e = Trust::default().open(b.path(), true).await.unwrap_err();
    assert!(e.to_string().contains("rotate the device CA"), "{e}");

    // Something that is not a key (the file keeps its 0600).
    let c = tempfile::tempdir().unwrap();
    let fc = TrustFiles::in_data_dir(c.path());
    plain_ca(c.path()).await;
    std::fs::write(&fc.key, "not pem").unwrap();
    let e = Trust::default().open(c.path(), true).await.unwrap_err();
    assert!(e.to_string().contains("is not a PEM file"), "{e}");
    std::fs::write(
        &fc.key,
        "-----BEGIN NOTHING-----\nAAAA\n-----END NOTHING-----\n",
    )
    .unwrap();
    let e = Trust::default().open(c.path(), true).await.unwrap_err();
    assert!(e.to_string().contains("'NOTHING'"), "{e}");
}

#[tokio::test]
async fn rotation_makes_a_new_ca_under_a_new_wrapping_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryWrapStore::default());
    let t = trust_with(&store);
    let old = t.open(dir.path(), true).await.unwrap();
    t.leaves
        .leaf(&old, "127.0.0.1".parse().unwrap(), 30, chrono::Utc::now())
        .unwrap();
    assert_eq!(t.leaves.leaves().len(), 1);

    let new = t.replace(dir.path()).await.unwrap();
    assert_ne!(new.fp(), old.fp());
    assert!(t.leaves.leaves().is_empty(), "the leaf cache is emptied");
    assert!(Arc::ptr_eq(&t.ca().unwrap(), &new));
    assert_eq!(store.count(dir.path()), 1, "the old item pruned");
    // A restart finds the new one, wrapped.
    let back = trust_with(&store).open(dir.path(), false).await.unwrap();
    assert_eq!(back.fp(), new.fp());
    assert_eq!(back.key_mode(), KeyMode::Wrapped);
}

#[tokio::test]
async fn the_status_reads_the_files_once_and_never_the_secret_service() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryWrapStore::default());
    let ca = trust_with(&store).open(dir.path(), true).await.unwrap();

    let t = trust_with(&store);
    let calls = store.calls();
    let st = t.status(dir.path()).ca;
    assert_eq!(st.state, DeviceCaState::Stored);
    assert_eq!(st.fp.as_deref(), Some(ca.fp()));
    assert_eq!(st.key_mode, Some(DeviceCaKeyMode::Wrapped));
    assert_eq!(
        st.created.as_deref(),
        Some(
            ca.facts()
                .created
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                .as_str()
        )
    );
    assert_eq!(store.calls(), calls, "no store call for the status");
    // Read once: a file removed afterwards changes nothing until a load.
    std::fs::remove_file(TrustFiles::in_data_dir(dir.path()).cert).unwrap();
    assert_eq!(t.status(dir.path()).ca.state, DeviceCaState::Stored);
}

#[tokio::test]
async fn nothing_shows_a_key() {
    let dir = tempfile::tempdir().unwrap();
    let files = TrustFiles::in_data_dir(dir.path());
    let store = Arc::new(MemoryWrapStore::default());
    store.set_unavailable(true);
    let t = trust_with(&store);
    let ca = t.open(dir.path(), true).await.unwrap();
    let leaf = t
        .leaves
        .leaf(&ca, "127.0.0.1".parse().unwrap(), 30, chrono::Utc::now())
        .unwrap();
    let pkcs8 = pem::parse(std::fs::read(&files.key).unwrap())
        .unwrap()
        .into_contents();
    let key_texts = [
        hex::encode(&pkcs8),
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &pkcs8),
        format!("{pkcs8:?}"),
    ];
    let wrap = tls::WrapKey::generate().unwrap();
    store.set_unavailable(false);
    let shown = [
        format!("{t:?}"),
        format!("{ca:?}"),
        format!("{leaf:?}"),
        format!("{store:?}"),
        format!("{wrap:?}"),
        serde_json::to_string(&t.status(dir.path())).unwrap(),
    ];
    for s in &shown {
        for k in &key_texts {
            assert!(!s.contains(k.as_str()), "{s}");
        }
        assert!(!s.contains(wrap.to_hex().as_str()), "{s}");
    }
    assert!(format!("{ca:?}").contains("[redacted]"));
}

/// The test gateways start with no Secret Service: a CA a test makes
/// through one is plain, so no test reaches the desktop's wallet.
#[tokio::test]
async fn a_test_gateway_never_reaches_the_secret_service() {
    let state = AppState::init_for_tests().await.unwrap();
    let ca = state
        .devices
        .trust
        .open(&state.data_dir, true)
        .await
        .unwrap();
    assert_eq!(ca.key_mode(), KeyMode::Plain);
}
