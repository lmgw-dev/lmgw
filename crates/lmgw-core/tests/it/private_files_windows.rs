//! The device CA's files and the main TLS key on Windows (Windows design
//! §4.4, WP6): checked by owner and DACL (`lmgw_private`), as Linux checks
//! them by uid and mode.

use std::process::Command;

use lmgw_core::devices::tls::{TrustError, TrustFiles};

fn icacls(path: &std::path::Path, grant: &str) {
    let out = Command::new("icacls")
        .arg(path)
        .args(["/grant", grant])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// What lmgw writes passes its own reads; a key another account may read,
/// and a `tls/` another account may change, are refused by name.
#[test]
fn the_cas_files_are_private_and_a_widened_one_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let files = TrustFiles::in_data_dir(data.path());
    files.ensure_dir().unwrap();
    files.check_dir().unwrap();
    files.write_key(b"-----BEGIN PRIVATE KEY-----\n").unwrap();
    files.write_cert(b"-----BEGIN CERTIFICATE-----\n").unwrap();
    assert_eq!(
        &*files.read_key().unwrap(),
        b"-----BEGIN PRIVATE KEY-----\n"
    );
    // Replaced atomically, still private.
    files.write_key(b"second").unwrap();
    assert_eq!(&*files.read_key().unwrap(), b"second");
    assert_eq!(files.read_cert().unwrap(), b"-----BEGIN CERTIFICATE-----\n");

    icacls(&files.key, "*S-1-5-32-545:(R)");
    match files.read_key() {
        Err(TrustError::BadMode(why)) => {
            assert!(
                why.contains("tls/ca.key is not private") && why.contains("Users"),
                "{why}"
            )
        }
        other => panic!("{other:?}"),
    }
    // Others reading tls/ is allowed (Linux: 0755 is), changing it is not.
    icacls(&files.dir, "*S-1-5-32-545:(RX)");
    files.check_dir().unwrap();
    icacls(&files.dir, "*S-1-5-32-545:(M)");
    match files.check_dir() {
        Err(TrustError::BadMode(why)) => assert!(why.contains("change it"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// A junction in `tls/`'s place is refused, not followed.
#[test]
fn a_junction_for_tls_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let elsewhere = data.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let files = TrustFiles::in_data_dir(data.path());
    let out = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&files.dir)
        .arg(&elsewhere)
        .output()
        .unwrap();
    assert!(out.status.success());
    match files.ensure_dir() {
        Err(TrustError::BadMode(why)) => {
            assert!(why.contains("not a directory of its own"), "{why}")
        }
        other => panic!("{other:?}"),
    }
}

/// The main TLS listener's key: a key others may read is named in the
/// pair's load, one only this account may read is not.
#[test]
fn the_main_tls_keys_readers_are_read_at_its_load() {
    let dir = tempfile::tempdir().unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["lmgw.example".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let (cert_path, key_path) = (dir.path().join("tls.crt"), dir.path().join("tls.key"));
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key.serialize_pem()).unwrap();
    let loaded = lmgw_core::server::main_tls::load(&cert_path, &key_path).unwrap();
    assert_eq!(loaded.key_not_private, None);
    icacls(&key_path, "*S-1-1-0:(R)");
    let loaded = lmgw_core::server::main_tls::load(&cert_path, &key_path).unwrap();
    let why = loaded.key_not_private.expect("Everyone may read it");
    assert!(why.contains("Everyone"), "{why}");
}
