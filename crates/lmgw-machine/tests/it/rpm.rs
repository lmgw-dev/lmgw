//! The rpm facts: who owns `nvidia-ctk` and who signed it, the imported
//! keys, and the database's change time (machine-check design §1.2, §3.6.1).

use std::path::Path;

use lmgw_machine::fake::FakeFacts;
use lmgw_machine::rpm::{
    database_changed, imported_keys, key_id_of, owner_of, parse_imported_keys, query, Owner,
};
use lmgw_machine::CmdOut;

const NVIDIA_FINGERPRINT: &str = "C95B 321B 61E8 8C18 09C4 F759 DDCA E044 F796 ECB0";

/// An rpm that knows `nvidia-container-toolkit-base` 1.20.1 owns
/// `/usr/bin/nvidia-ctk`, signed by NVIDIA's key.
fn rpm_machine() -> FakeFacts {
    let f = FakeFacts::new();
    f.file("/usr/bin/nvidia-ctk", b"")
        .file("/usr/bin/by-hand", b"");
    f.program("/usr/bin/rpm", |args| {
        let target = args.last().map(String::as_str).unwrap_or("");
        Ok(match (args[0].as_str(), target) {
            ("-qf", "/usr/bin/nvidia-ctk") => CmdOut::success(
                "nvidia-container-toolkit-base\t(none)\t1.20.1\t1\tx86_64\tRSA/SHA512, Fri Sep 18 \
                 23:09:15 2026, Key ID ddcae044f796ecb0\t(none)\n",
            ),
            ("-qf", "/usr/bin/by-hand") => {
                CmdOut::failure(1, "file /usr/bin/by-hand is not owned by any package\n")
            }
            ("-q", "golang-github-nvidia-container-toolkit") => CmdOut::failure(
                1,
                "package golang-github-nvidia-container-toolkit is not installed\n",
            ),
            ("-q", "unsigned") => {
                CmdOut::success("unsigned\t2\t1.0\t3.fc44\tnoarch\t(none)\t(none)\n")
            }
            _ => CmdOut::failure(1, "error: unexpected query\n"),
        })
    });
    f
}

#[test]
fn the_owner_of_nvidia_ctk_carries_its_signing_key() {
    let f = rpm_machine();
    let Owner::Packages(pkgs) = owner_of(&f, Path::new("/usr/bin/nvidia-ctk")).unwrap() else {
        panic!("not owned?")
    };
    assert_eq!(pkgs.len(), 1);
    let p = &pkgs[0];
    assert_eq!(p.name, "nvidia-container-toolkit-base");
    assert_eq!(p.epoch, None);
    assert_eq!(p.nvra(), "nvidia-container-toolkit-base-1.20.1-1.x86_64");
    assert_eq!(p.sig_key_id.as_deref(), Some("ddcae044f796ecb0"));
    // The C locale on every query, so the signature line parses anywhere.
    let calls = f.calls_of("/usr/bin/rpm");
    assert_eq!(calls[0][0], "-qf");
}

#[test]
fn a_file_put_there_by_hand_is_not_owned_and_a_missing_one_is_absent() {
    let f = rpm_machine();
    assert_eq!(
        owner_of(&f, Path::new("/usr/bin/by-hand")).unwrap(),
        Owner::NotOwned
    );
    assert_eq!(
        owner_of(&f, Path::new("/usr/bin/nothing")).unwrap(),
        Owner::Absent
    );
    assert!(
        f.calls_of("/usr/bin/rpm")
            .iter()
            .all(|a| a.last().map(String::as_str) != Some("/usr/bin/nothing")),
        "an absent file is not asked about"
    );
}

#[test]
fn a_machine_without_rpm_says_so() {
    let f = FakeFacts::new();
    f.file("/usr/bin/nvidia-ctk", b"");
    assert_eq!(
        owner_of(&f, Path::new("/usr/bin/nvidia-ctk")).unwrap(),
        Owner::NoRpm
    );
    assert_eq!(query(&f, "anything").unwrap(), Vec::new());
}

#[test]
fn a_package_query_reads_epoch_and_a_missing_signature() {
    let f = rpm_machine();
    assert!(query(&f, "golang-github-nvidia-container-toolkit")
        .unwrap()
        .is_empty());
    let p = &query(&f, "unsigned").unwrap()[0];
    assert_eq!(p.epoch.as_deref(), Some("2"));
    assert_eq!(p.sig_key_id, None);
}

#[test]
fn key_ids_come_out_of_both_signature_renderings() {
    assert_eq!(
        key_id_of("RSA/SHA256, Tue Jan 20 09:22:11 2026, Key ID DBFCF71C6D9F90A6").as_deref(),
        Some("dbfcf71c6d9f90a6")
    );
    assert_eq!(key_id_of("(none)"), None);
}

#[test]
fn imported_keys_are_read_from_rpm_6_and_older_listings() {
    let rpm6 = parse_imported_keys(
        "ab2985bde164897ba46ea69a234faacb575b15d1 Example Vendor <keys@example.com> public key\n\
         c95b321b61e88c1809c4f759ddcae044f796ecb0 NVIDIA CORPORATION (Open Source Projects) \
         <cudatools@nvidia.com> public key\n",
    );
    assert!(rpm6.contains(NVIDIA_FINGERPRINT));
    assert!(!rpm6.contains("0000 0000 0000 0000 0000 0000 0000 0000 0000 0000"));

    let older = parse_imported_keys(
        "gpg-pubkey-f796ecb0-5f1b0e9e: NVIDIA CORPORATION (Open Source Projects) public key\n",
    );
    assert!(older.contains(NVIDIA_FINGERPRINT));

    let without = parse_imported_keys("ab2985bde164897ba46ea69a234faacb575b15d1 x public key\n");
    assert!(!without.contains(NVIDIA_FINGERPRINT));
}

#[test]
fn rpmkeys_list_runs_by_absolute_path() {
    let f = FakeFacts::new();
    f.reply(
        "/usr/bin/rpmkeys",
        CmdOut::success("c95b321b61e88c1809c4f759ddcae044f796ecb0 NVIDIA public key\n"),
    );
    assert!(imported_keys(&f).unwrap().contains(NVIDIA_FINGERPRINT));
    assert_eq!(
        f.calls_of("/usr/bin/rpmkeys"),
        vec![vec!["--list".to_string()]]
    );
}

#[test]
fn the_database_change_time_is_the_newer_of_the_sqlite_files_and_never_shm() {
    let f = FakeFacts::new();
    let dir = "/usr/lib/sysimage/rpm";
    f.file(format!("{dir}/rpmdb.sqlite"), b"")
        .file(format!("{dir}/rpmdb.sqlite-wal"), b"")
        .file(format!("{dir}/rpmdb.sqlite-shm"), b"")
        .touch(format!("{dir}/rpmdb.sqlite"), 100)
        .touch(format!("{dir}/rpmdb.sqlite-wal"), 200)
        .touch(format!("{dir}/rpmdb.sqlite-shm"), 900);
    let (file, at) = database_changed(&f).unwrap();
    assert_eq!(at, 200);
    assert!(file.ends_with("rpmdb.sqlite-wal"), "{}", file.display());

    // A read touches only -shm: nothing changed.
    f.touch(format!("{dir}/rpmdb.sqlite-shm"), 1000);
    assert_eq!(database_changed(&f).unwrap().1, 200);

    assert_eq!(database_changed(&FakeFacts::new()), None);
}
