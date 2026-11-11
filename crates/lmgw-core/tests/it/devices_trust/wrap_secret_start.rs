//! A container-mode start and the image's wrapping key (headless server
//! design §9, WP8, D102, D103): `Trust::start_on_secret` installs the
//! secret, wraps a plain key at once when the secret holds one (the device
//! listener on or off), and says in one line, at its level, what it found
//! and did — read back through the test's own log capture.

use std::path::{Path, PathBuf};

use lmgw_api_types::device_link::{DeviceCaKeyMode, DeviceCaState};
use lmgw_core::devices::tls::{
    KeyMode, Trust, TrustFiles, WrapKey, WrapKeySecret, PLAIN_LABEL, WRAPPED_LABEL,
    WRAP_KEY_SECRET_HOW,
};

use crate::common::captured_log::capture_log;

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

fn label(p: &Path) -> String {
    pem::parse(std::fs::read(p).unwrap())
        .unwrap()
        .tag()
        .to_string()
}

fn write_key(s: &Scratch) {
    let hex = WrapKey::generate().unwrap().to_hex();
    std::fs::write(&s.secret, format!("{}\n", hex.as_str())).unwrap();
}

/// A CA in the scratch data dir with its key plain, as a start without the
/// secret makes it.
async fn plain_ca(s: &Scratch) -> String {
    let t = Trust::default();
    t.set_store(std::sync::Arc::new(WrapKeySecret::read(&s.secret)));
    let ca = t.open(&s.data, true).await.unwrap();
    assert_eq!(label(&s.files.key), PLAIN_LABEL);
    ca.fp().to_string()
}

/// A CA in the scratch data dir with its key wrapped under the secret
/// there now.
async fn wrapped_ca(s: &Scratch) -> String {
    let t = Trust::default();
    t.set_store(std::sync::Arc::new(WrapKeySecret::read(&s.secret)));
    let ca = t.open(&s.data, true).await.unwrap();
    assert_eq!(label(&s.files.key), WRAPPED_LABEL);
    ca.fp().to_string()
}

/// The start's step on the scratch dir, captured: the trust it left and
/// the log's text.
async fn start(s: &Scratch) -> (Trust, String) {
    let (log, guard) = capture_log();
    let t = Trust::default();
    t.start_on_secret(&s.data, WrapKeySecret::read(&s.secret))
        .await;
    drop(guard);
    (t, log.text())
}

/// The one captured line holding every one of `needles`, at `level`
/// (`INFO`, `WARN`, `ERROR`), and no line above INFO otherwise.
fn the_line(text: &str, level: &str, needles: &[&str]) {
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| needles.iter().all(|n| l.contains(n)))
        .collect();
    assert_eq!(lines.len(), 1, "{needles:?} in:\n{text}");
    assert!(
        lines[0].contains(&format!(" {level} ")) || lines[0].starts_with(&format!("{level} ")),
        "{level}: {}",
        lines[0]
    );
    for other in ["WARN", "ERROR"].into_iter().filter(|l| *l != level) {
        assert!(
            !text.lines().any(|l| l.contains(&format!(" {other} "))),
            "no {other} line:\n{text}"
        );
    }
}

#[tokio::test]
async fn no_ca_yet_and_the_secret_says_the_first_ca_is_wrapped() {
    let s = scratch();
    write_key(&s);
    let (_, text) = start(&s).await;
    the_line(
        &text,
        "INFO",
        &[
            "has no device CA yet",
            "wrapped under the wrapping key secret",
            &s.secret.display().to_string(),
        ],
    );
    assert!(!s.files.key.exists(), "nothing is made at the start");
}

#[tokio::test]
async fn no_ca_yet_and_no_secret_says_plain_and_how_to_make_it() {
    let s = scratch();
    let (_, text) = start(&s).await;
    the_line(
        &text,
        "INFO",
        &[
            "no wrapping key secret is mounted at",
            "kept plain (0600)",
            WRAP_KEY_SECRET_HOW,
        ],
    );
}

/// The reviewer's case: a plain key on disk, the device listener off (no
/// one opens the CA), the secret there. After the start the key on disk is
/// wrapped, the CA the same, and the status shows it stored and wrapped,
/// not loaded.
#[tokio::test]
async fn a_plain_key_is_wrapped_at_the_start_with_the_listener_off() {
    let s = scratch();
    let fp = plain_ca(&s).await;
    let pkcs8 = pem::parse(std::fs::read(&s.files.key).unwrap())
        .unwrap()
        .into_contents();
    write_key(&s);

    let (t, text) = start(&s).await;
    the_line(
        &text,
        "INFO",
        &[
            "is wrapped under the wrapping key secret",
            "now",
            "the plain file was replaced",
        ],
    );
    assert_eq!(label(&s.files.key), WRAPPED_LABEL);
    assert_eq!(Trust::key_mode_on_disk(&s.data), Some(KeyMode::Wrapped));
    let sealed = pem::parse(std::fs::read(&s.files.key).unwrap())
        .unwrap()
        .into_contents();
    assert!(
        !sealed
            .windows(16)
            .any(|w| pkcs8.windows(16).any(|p| p == w)),
        "no part of the plain key is left in the file"
    );
    let st = t.status(&s.data).ca;
    assert_eq!(st.state, DeviceCaState::Stored, "not loaded by the start");
    assert_eq!(st.key_mode, Some(DeviceCaKeyMode::Wrapped));
    assert_eq!(st.fp.as_deref(), Some(fp.as_str()));
    // The listener's later open finds it wrapped and the same CA.
    let ca = t.open(&s.data, false).await.unwrap();
    assert_eq!(ca.fp(), fp);
    assert_eq!(ca.key_mode(), KeyMode::Wrapped);
}

#[tokio::test]
async fn a_plain_key_that_cannot_be_wrapped_is_a_warn_line_and_stays_as_it_was() {
    let s = scratch();
    plain_ca(&s).await;
    // Another CA's certificate beside the key: the load refuses the pair,
    // so nothing is rewritten.
    let other = scratch();
    plain_ca(&other).await;
    std::fs::copy(&other.files.cert, &s.files.cert).unwrap();
    write_key(&s);
    let before = std::fs::read(&s.files.key).unwrap();
    let (_, text) = start(&s).await;
    the_line(
        &text,
        "WARN",
        &["is plain, and this start could not wrap it", "tries again"],
    );
    assert_eq!(std::fs::read(&s.files.key).unwrap(), before);
}

#[tokio::test]
async fn a_key_file_others_can_read_is_a_warn_line_and_left_as_it_is() {
    use std::os::unix::fs::PermissionsExt;
    let s = scratch();
    plain_ca(&s).await;
    write_key(&s);
    std::fs::set_permissions(&s.files.key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let before = std::fs::read(&s.files.key).unwrap();
    let (_, text) = start(&s).await;
    the_line(
        &text,
        "WARN",
        &["key file is not usable", "mode 0644", "leaves it as it is"],
    );
    assert_eq!(std::fs::read(&s.files.key).unwrap(), before);
}

#[tokio::test]
async fn an_already_wrapped_key_and_its_secret_is_an_info_line_and_untouched() {
    let s = scratch();
    write_key(&s);
    wrapped_ca(&s).await;
    let before = std::fs::read(&s.files.key).unwrap();
    let (_, text) = start(&s).await;
    the_line(
        &text,
        "INFO",
        &[
            "is wrapped under the wrapping key secret",
            "opens it when it starts",
        ],
    );
    assert_eq!(std::fs::read(&s.files.key).unwrap(), before);
}

#[tokio::test]
async fn a_wrapped_key_and_no_secret_is_a_warn_line_naming_it() {
    let s = scratch();
    write_key(&s);
    wrapped_ca(&s).await;
    std::fs::remove_file(&s.secret).unwrap();
    let (_, text) = start(&s).await;
    the_line(
        &text,
        "WARN",
        &[
            "is wrapped, and no wrapping key secret is mounted",
            &s.secret.display().to_string(),
            "Restore the Podman secret",
            "rotate the device CA",
        ],
    );
}

#[tokio::test]
async fn a_plain_key_and_no_secret_is_a_warn_line_with_how_to_make_it() {
    let s = scratch();
    plain_ca(&s).await;
    let (_, text) = start(&s).await;
    the_line(
        &text,
        "WARN",
        &[
            "is plain (0600)",
            "no wrapping key secret is mounted",
            WRAP_KEY_SECRET_HOW,
        ],
    );
    assert_eq!(label(&s.files.key), PLAIN_LABEL);
}

#[tokio::test]
async fn a_secret_that_holds_no_key_is_an_error_line_and_touches_nothing() {
    let s = scratch();
    plain_ca(&s).await;
    std::fs::write(&s.secret, b"abcd\n").unwrap();
    let (_, text) = start(&s).await;
    the_line(
        &text,
        "ERROR",
        &[
            "holds 4 hex digits",
            "cannot make, wrap or open the device CA",
        ],
    );
    assert_eq!(label(&s.files.key), PLAIN_LABEL);
}
