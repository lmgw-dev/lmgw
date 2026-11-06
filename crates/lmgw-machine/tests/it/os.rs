//! os-release and image-based systems (machine-check design §3.7).

use lmgw_machine::fake::FakeFacts;
use lmgw_machine::os::{os_release, ostree_booted, parse_os_release};

#[test]
fn fedora_kde_reads_its_id_and_version() {
    let os = parse_os_release(
        "NAME=\"Fedora Linux\"\nVERSION=\"44 (KDE Plasma Desktop Edition)\"\nID=fedora\n\
         VERSION_ID=44\nPRETTY_NAME=\"Fedora Linux 44 (KDE Plasma Desktop Edition)\"\n",
    );
    assert_eq!(os.id, "fedora");
    assert_eq!(os.version_id.as_deref(), Some("44"));
    assert_eq!(
        os.pretty_name.as_deref(),
        Some("Fedora Linux 44 (KDE Plasma Desktop Edition)")
    );
    assert!(os.is_like("fedora"));
}

#[test]
fn id_like_lists_the_families() {
    let os = parse_os_release("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID='9.6'\n");
    assert_eq!(os.id, "rocky");
    assert!(os.is_like("rhel") && os.is_like("fedora"));
    assert!(!os.is_like("debian"));
    assert_eq!(os.version_id.as_deref(), Some("9.6"));
}

#[test]
fn usr_lib_os_release_is_the_fallback_and_ostree_is_seen() {
    let f = FakeFacts::new();
    f.file("/usr/lib/os-release", "ID=fedora\nVARIANT_ID=kinoite\n")
        .file("/run/ostree-booted", b"");
    assert_eq!(os_release(&f).unwrap().id, "fedora");
    assert!(ostree_booted(&f));
    assert!(os_release(&FakeFacts::new()).is_none());
    assert!(!ostree_booted(&FakeFacts::new()));
}
