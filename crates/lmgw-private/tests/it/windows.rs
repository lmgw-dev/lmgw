//! The check and the creates on a real NTFS directory.

use std::io::Write;
use std::process::Command;

use lmgw_private::{
    check_path, create_dir, create_new_file, make_private, open_no_follow, NotPrivate, OpenError,
    Others,
};

/// `icacls` with `args`, which must succeed.
fn icacls(args: &[&str]) {
    let out = Command::new("icacls").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "icacls {args:?}: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// A directory made private holds a key file made private: both pass the
/// strict check, and the file is not opened through a link.
#[test]
fn what_lmgw_makes_is_private() {
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join("tls");
    create_dir(&dir).unwrap();
    assert!(check_path(&dir, Others::Nothing).unwrap().is_dir);
    let key = dir.join("ca.key");
    let mut f = create_new_file(&key).unwrap();
    f.write_all(b"secret").unwrap();
    drop(f);
    assert!(!check_path(&key, Others::Nothing).unwrap().is_dir);
    // A second exclusive create is refused, never truncating.
    let e = create_new_file(&key).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&key).unwrap(), b"secret");
    let opened = open_no_follow(&key, false).unwrap();
    use std::os::windows::io::AsHandle;
    lmgw_private::check_handle(opened.as_handle(), Others::Nothing).unwrap();
}

/// The temp dir under the profile, as Windows made it, is private: the
/// profile's own entries (the user, SYSTEM, the Administrators) are all it
/// grants. This is the assumption the design's "a per-user directory under
/// the profile" rests on, checked on the machine the tests run on.
#[test]
fn the_profiles_temp_dir_is_private_as_windows_made_it() {
    let base = tempfile::tempdir().unwrap();
    let plain = base.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    check_path(&plain, Others::Nothing).unwrap_or_else(|e| {
        panic!(
            "{} (inherited from the profile) is not private: {e}",
            plain.display()
        )
    });
}

/// Read access for Users is refused for a key and allowed for a directory
/// whose files are what is private; write access is refused for both.
/// make_private closes it again.
#[test]
fn another_account_with_a_right_is_refused_and_make_private_closes_it() {
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join("d");
    create_dir(&dir).unwrap();
    let d = dir.to_str().unwrap();
    icacls(&[d, "/grant", "*S-1-5-32-545:(R)"]);
    match check_path(&dir, Others::Nothing) {
        Err(NotPrivate::Others { change: false, who }) => {
            assert!(who.iter().any(|w| w.contains("Users")), "{who:?}")
        }
        other => panic!("{other:?}"),
    }
    check_path(&dir, Others::ReadOnly).unwrap();
    icacls(&[d, "/grant", "*S-1-5-32-545:(M)"]);
    assert!(matches!(
        check_path(&dir, Others::ReadOnly),
        Err(NotPrivate::Others { change: true, .. })
    ));
    make_private(&dir).unwrap();
    check_path(&dir, Others::Nothing).unwrap();
}

/// A symbolic link (a junction where creating one needs no privilege) is
/// refused by the check and by the opens, never followed.
#[test]
fn a_junction_is_refused_not_followed() {
    let base = tempfile::tempdir().unwrap();
    let real = base.path().join("real");
    create_dir(&real).unwrap();
    let link = base.path().join("link");
    let out = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&real)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(matches!(
        check_path(&link, Others::Nothing),
        Err(NotPrivate::Link)
    ));
    assert!(matches!(make_private(&link), Err(OpenError::Link)));
    // An exclusive create where the junction stands is refused (Windows
    // answers access denied, the name being a directory), and nothing is
    // made through it.
    assert!(create_new_file(&link).is_err());
    assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
}
