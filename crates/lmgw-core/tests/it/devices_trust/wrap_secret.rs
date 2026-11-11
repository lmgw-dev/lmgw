//! The server image's wrapping key (headless server design §9, WP8): the
//! fixed key of a Podman secret, read from a test's own file in place of
//! `/run/secrets/lmgw-wrap-key`. It wraps a new CA and a plain one, adds
//! and prunes nothing, and every way it can be missing, replaced or wrong
//! leaves the CA down with a sentence that names the secret.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lmgw_api_types::device_link::{DeviceCaErrorKind, DeviceCaKeyMode, DeviceCaState};
use lmgw_core::devices::tls::{
    KeyMode, Trust, TrustError, TrustFiles, WrapKey, WrapKeySecret, WrapKeyStore, PLAIN_LABEL,
    WRAPPED_LABEL,
};

use super::mode_of;

fn label(p: &Path) -> String {
    pem::parse(std::fs::read(p).unwrap())
        .unwrap()
        .tag()
        .to_string()
}

/// A secret file as `openssl rand -hex 32` writes it: 64 hex digits and a
/// newline.
fn write_secret(path: &Path, hex: &str) {
    std::fs::write(path, format!("{hex}\n")).unwrap();
}

fn random_hex() -> String {
    WrapKey::generate().unwrap().to_hex().to_string()
}

/// A trust whose store is the secret at `path`, as a container-mode start
/// reads it.
fn trust_on(path: &Path) -> Trust {
    let t = Trust::default();
    t.set_store(Arc::new(WrapKeySecret::read(path)));
    t
}

struct Scratch {
    _dir: tempfile::TempDir,
    data: PathBuf,
    secret: PathBuf,
    files: TrustFiles,
}

fn scratch() -> Scratch {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let secret = dir.path().join("lmgw-wrap-key");
    let files = TrustFiles::in_data_dir(&data);
    Scratch {
        _dir: dir,
        data,
        secret,
        files,
    }
}

#[tokio::test]
async fn the_secret_wraps_a_new_ca_and_opens_it_at_every_start() {
    let s = scratch();
    write_secret(&s.secret, &random_hex());
    let before = std::fs::read(&s.secret).unwrap();

    let ca = trust_on(&s.secret).open(&s.data, true).await.unwrap();
    assert_eq!(ca.key_mode(), KeyMode::Wrapped);
    assert_eq!(label(&s.files.key), WRAPPED_LABEL);
    assert_eq!(mode_of(&s.files.key), 0o600);
    // Nothing is added to it or pruned from it: the file is as it was.
    assert_eq!(std::fs::read(&s.secret).unwrap(), before);

    // The next start, the same secret: the same CA.
    let again = trust_on(&s.secret).open(&s.data, false).await.unwrap();
    assert_eq!(again.fp(), ca.fp());
    assert_eq!(again.key_mode(), KeyMode::Wrapped);

    // A rotation seals the new key under the same secret.
    let t = trust_on(&s.secret);
    let rotated = t.replace(&s.data).await.unwrap();
    assert_ne!(rotated.fp(), ca.fp());
    assert_eq!(rotated.key_mode(), KeyMode::Wrapped);
    let reopened = trust_on(&s.secret).open(&s.data, false).await.unwrap();
    assert_eq!(reopened.fp(), rotated.fp());
    assert_eq!(std::fs::read(&s.secret).unwrap(), before);
}

#[tokio::test]
async fn a_plain_key_is_wrapped_at_the_first_start_with_the_secret() {
    let s = scratch();
    // A start without the secret keeps it plain.
    let ca = trust_on(&s.secret).open(&s.data, true).await.unwrap();
    assert_eq!(ca.key_mode(), KeyMode::Plain);
    assert_eq!(label(&s.files.key), PLAIN_LABEL);
    let pkcs8 = pem::parse(std::fs::read(&s.files.key).unwrap())
        .unwrap()
        .into_contents();

    // The owner creates the secret: the next start wraps the key.
    write_secret(&s.secret, &random_hex());
    let next = trust_on(&s.secret);
    assert_eq!(
        next.status(&s.data).ca.key_mode,
        Some(DeviceCaKeyMode::Plain)
    );
    let wrapped = next.open(&s.data, false).await.unwrap();
    assert_eq!(wrapped.key_mode(), KeyMode::Wrapped);
    assert_eq!(wrapped.fp(), ca.fp());
    assert_eq!(label(&s.files.key), WRAPPED_LABEL);
    assert_eq!(mode_of(&s.files.key), 0o600);
    let sealed = pem::parse(std::fs::read(&s.files.key).unwrap())
        .unwrap()
        .into_contents();
    assert!(
        !sealed
            .windows(16)
            .any(|w| pkcs8.windows(16).any(|p| p == w)),
        "no part of the plain key is left in the file"
    );
    assert_eq!(
        Trust::key_mode_on_disk(&s.data),
        Some(KeyMode::Wrapped),
        "the start's line reads the label"
    );
    let third = trust_on(&s.secret).open(&s.data, false).await.unwrap();
    assert_eq!(third.key_mode(), KeyMode::Wrapped);
}

#[tokio::test]
async fn a_removed_secret_leaves_a_wrapped_ca_down_by_name_until_restored_or_rotated() {
    let s = scratch();
    let hex = random_hex();
    write_secret(&s.secret, &hex);
    let ca = trust_on(&s.secret).open(&s.data, true).await.unwrap();

    // The secret removed (or the unit's Secret= line).
    std::fs::remove_file(&s.secret).unwrap();
    let t = trust_on(&s.secret);
    let e = t.open(&s.data, true).await.unwrap_err();
    assert!(matches!(e, TrustError::WrapKeyMissing(_)), "{e}");
    let st = t.status(&s.data).ca;
    assert_eq!(st.state, DeviceCaState::Error);
    assert_eq!(
        st.fp.as_deref(),
        Some(ca.fp()),
        "what the files show is shown"
    );
    let err = st.error.unwrap();
    assert_eq!(err.kind, DeviceCaErrorKind::WrapKeyMissing);
    let secret = s.secret.display().to_string();
    assert!(err.message.contains(&secret), "{}", err.message);
    assert!(err.message.contains("lmgw-wrap-key"), "{}", err.message);
    assert!(!err.message.contains("Secret Service"), "{}", err.message);
    // `create` never replaces a CA that exists.
    assert_eq!(label(&s.files.key), WRAPPED_LABEL);

    // Restored: it opens again.
    write_secret(&s.secret, &hex);
    let back = trust_on(&s.secret).open(&s.data, false).await.unwrap();
    assert_eq!(back.fp(), ca.fp());

    // Removed again, and the owner rotates: a new key, kept plain.
    std::fs::remove_file(&s.secret).unwrap();
    let t = trust_on(&s.secret);
    let rotated = t.replace(&s.data).await.unwrap();
    assert_ne!(rotated.fp(), ca.fp());
    assert_eq!(rotated.key_mode(), KeyMode::Plain);
    assert_eq!(label(&s.files.key), PLAIN_LABEL);
}

#[tokio::test]
async fn another_secret_does_not_open_the_key_and_says_so() {
    let s = scratch();
    write_secret(&s.secret, &random_hex());
    let ca = trust_on(&s.secret).open(&s.data, true).await.unwrap();
    write_secret(&s.secret, &random_hex());
    let t = trust_on(&s.secret);
    let e = t.open(&s.data, false).await.unwrap_err();
    assert!(matches!(e, TrustError::Unreadable(_)), "{e}");
    let msg = e.to_string();
    assert!(msg.contains(&s.secret.display().to_string()), "{msg}");
    assert!(msg.contains("does not open it"), "{msg}");
    // Nothing on disk changed.
    assert_eq!(label(&s.files.key), WRAPPED_LABEL);
    assert_eq!(t.status(&s.data).ca.fp.as_deref(), Some(ca.fp()));
}

/// One key seals every write (D98, D99), so a rotation alone does not end
/// an older copy of the key file: a volume backup taken before it still
/// opens with the same secret. The recovery when such a backup leaked with
/// the secret: replace the secret, restart (the key file is unreadable
/// then), rotate, and the new key is sealed under the new secret, which
/// opens neither the leaked copy nor anything the old secret sealed.
#[tokio::test]
async fn a_leaked_backup_ends_with_a_new_secret_and_a_rotation_not_a_rotation_alone() {
    let s = scratch();
    let old = random_hex();
    write_secret(&s.secret, &old);
    let first = trust_on(&s.secret).open(&s.data, true).await.unwrap();
    // The backup: the data volume's tls/ as it was.
    let backup = s.data.with_file_name("backup");
    std::fs::create_dir(&backup).unwrap();
    let leaked = TrustFiles::in_data_dir(&backup);
    std::fs::create_dir(&leaked.dir).unwrap();
    std::fs::copy(&s.files.key, &leaked.key).unwrap();
    std::fs::copy(&s.files.cert, &leaked.cert).unwrap();
    let restrict = |p: &Path, mode| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    restrict(&leaked.dir, 0o700);
    restrict(&leaked.key, 0o600);

    // A rotation under the same secret: the live CA is new, and the
    // backup's old key still opens with that secret.
    let rotated = trust_on(&s.secret).replace(&s.data).await.unwrap();
    assert_ne!(rotated.fp(), first.fp());
    let still = trust_on(&s.secret).open(&backup, false).await.unwrap();
    assert_eq!(still.fp(), first.fp(), "the same secret opens the old copy");

    // The recovery, step 1: the secret replaced, then a restart. The key
    // file on the volume is sealed under the old secret: unreadable, said
    // with the secret's path, and nothing on disk changes.
    write_secret(&s.secret, &random_hex());
    let restarted = trust_on(&s.secret);
    let e = restarted.open(&s.data, false).await.unwrap_err();
    assert!(matches!(e, TrustError::Unreadable(_)), "{e}");
    assert!(
        e.to_string().contains(&s.secret.display().to_string()),
        "{e}"
    );
    assert_eq!(
        restarted.status(&s.data).ca.error.unwrap().kind,
        DeviceCaErrorKind::Unreadable
    );

    // Step 2: the owner rotates on that start; the new key is sealed under
    // the new secret.
    let recovered = restarted.replace(&s.data).await.unwrap();
    assert_ne!(recovered.fp(), rotated.fp());
    assert_eq!(recovered.key_mode(), KeyMode::Wrapped);
    assert_eq!(label(&s.files.key), WRAPPED_LABEL);
    let next = trust_on(&s.secret).open(&s.data, false).await.unwrap();
    assert_eq!(next.fp(), recovered.fp());

    // The leaked copy does not open under the new secret, and the old
    // secret (which a leak may also have carried) opens nothing on the
    // volume.
    let e = trust_on(&s.secret).open(&backup, false).await.unwrap_err();
    assert!(matches!(e, TrustError::Unreadable(_)), "{e}");
    let old_file = s.secret.with_file_name("old-secret");
    write_secret(&old_file, &old);
    let e = trust_on(&old_file).open(&s.data, false).await.unwrap_err();
    assert!(matches!(e, TrustError::Unreadable(_)), "{e}");
}

/// The file's content is checked against the key's real length, 64 hex
/// digits, and every way it is wrong is said with what was found; none of
/// it makes, wraps or keeps a key plain.
#[tokio::test]
async fn a_secret_that_holds_no_key_is_refused_with_what_is_wrong() {
    let digits = random_hex();
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("empty", Vec::new(), "holds 0 hex digits"),
        (
            "short",
            digits.as_bytes()[..63].to_vec(),
            "holds 63 hex digits",
        ),
        (
            "long",
            format!("{digits}{digits}").into_bytes(),
            "holds 128 hex digits",
        ),
        (
            "raw bytes",
            (0u8..32).map(|b| b.wrapping_mul(37) | 0x80).collect(),
            "not hex digits",
        ),
        (
            "a passphrase",
            b"correct horse battery staple".to_vec(),
            "not hex digits",
        ),
    ];
    for (what, bytes, says) in cases {
        let s = scratch();
        std::fs::write(&s.secret, &bytes).unwrap();
        let found = WrapKeySecret::read(&s.secret);
        let why = found.problem().unwrap_or_else(|| panic!("{what}: refused"));
        assert!(why.contains(says), "{what}: {why}");
        assert!(why.contains("64"), "{what}: {why}");
        assert!(
            why.contains("podman secret create lmgw-wrap-key"),
            "{what}: {why}"
        );
        assert!(matches!(found.fixed_key(), Some(Err(_))));

        // No CA is made under it, plain or otherwise, and the kind is the
        // secret's own, not the Secret Service's.
        let t = trust_on(&s.secret);
        let e = t.open(&s.data, true).await.unwrap_err();
        assert!(matches!(e, TrustError::WrapKeySecret(_)), "{what}: {e}");
        assert_eq!(
            t.status(&s.data).ca.error.unwrap().kind,
            DeviceCaErrorKind::WrapKeySecret
        );
        assert!(!s.files.key.exists(), "{what}");
    }

    // A plain key stays as it is, and the CA down, while the secret is
    // wrong.
    let s = scratch();
    let ca = trust_on(&s.secret).open(&s.data, true).await.unwrap();
    assert_eq!(ca.key_mode(), KeyMode::Plain);
    std::fs::write(&s.secret, b"abcd\n").unwrap();
    let e = trust_on(&s.secret).open(&s.data, false).await.unwrap_err();
    assert!(e.to_string().contains("holds 4 hex digits"), "{e}");
    assert_eq!(label(&s.files.key), PLAIN_LABEL);

    // A directory where the file should be: unreadable, said so.
    let s = scratch();
    std::fs::create_dir(&s.secret).unwrap();
    let found = WrapKeySecret::read(&s.secret);
    assert!(
        found.problem().unwrap().contains("cannot be read"),
        "{:?}",
        found.problem()
    );
}

#[test]
fn uppercase_and_surrounding_whitespace_are_a_key_and_debug_never_shows_it() {
    let s = scratch();
    let hex = random_hex();
    std::fs::write(&s.secret, format!("  {}\r\n\n", hex.to_uppercase())).unwrap();
    let found = WrapKeySecret::read(&s.secret);
    assert!(found.has_key(), "{:?}", found.problem());
    assert!(matches!(found.fixed_key(), Some(Ok(_))));
    assert_eq!(found.secret_file(), Some(s.secret.as_path()));
    let shown = format!("{found:?}");
    assert!(!shown.to_lowercase().contains(&hex[..16]), "{shown}");
    assert!(shown.contains("redacted"), "{shown}");

    let none = WrapKeySecret::read(&s.secret.with_file_name("absent"));
    assert!(none.is_absent() && none.problem().is_none() && !none.has_key());
    assert!(matches!(
        none.fixed_key(),
        Some(Err(lmgw_core::devices::tls::StoreError::Unavailable(_)))
    ));
}
