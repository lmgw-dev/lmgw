//! Which section the toolkit comes from, and lmgw's own two files (§3.2,
//! "Where the toolkit comes from", "What S1 writes", "Which sections
//! count", "An existing NVIDIA section").

use lmgw_machine::plan::embedded::{KEY_FILE, REPO_FILE, REPO_MARKER, REPO_TEXT};
use lmgw_machine::plan::{Origin, OwnFile, Provider, Step, Want, PROVIDERS};

use super::machine::{kinds, Machine, NVIDIA_REPO, NVIDIA_REPO_PATH};

/// The person's NVIDIA section is used as it is: never rewritten (no S1),
/// and the install gets `<scope>`'s package checks against the pinned key,
/// although their file says `gpgcheck=0`.
#[test]
fn an_existing_nvidia_section_is_used_as_it_is_with_package_checks() {
    let m = Machine::fedora();
    m.nvidia_section();
    let p = m.plan();
    assert_eq!(kinds(&p), ["s2_add_key", "s4_install"]);
    let argv = &p.steps[1].commands()[0];
    for opt in [
        "--repo=nvidia-container-toolkit",
        "--setopt=nvidia-container-toolkit.skip_if_unavailable=False",
        "--setopt=nvidia-container-toolkit.gpgcheck=1",
        "--setopt=nvidia-container-toolkit.gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit",
    ] {
        assert!(argv.iter().any(|a| a == opt), "{opt} missing from {argv:?}");
    }
    assert_eq!(p.section.unwrap().origin, Origin::Person);
    // The experimental section is NVIDIA's too, and never chosen.
    assert!(!argv.iter().any(|a| a.contains("experimental")));
}

/// A switched-off NVIDIA section is switched on through dnf's own command
/// (S1′), which leaves the person's file alone.
#[test]
fn a_switched_off_nvidia_section_is_switched_on() {
    let m = Machine::fedora();
    m.f.file(
        NVIDIA_REPO_PATH,
        NVIDIA_REPO.replacen("enabled=1", "enabled=0", 1),
    );
    let p = m.plan();
    assert_eq!(kinds(&p), ["s1_enable_repo", "s2_add_key", "s4_install"]);
    assert_eq!(
        p.steps[0].commands(),
        [[
            "/usr/bin/dnf5",
            "config-manager",
            "setopt",
            "nvidia-container-toolkit.enabled=1"
        ]]
    );
    assert!(
        p.steps[0].file().is_none(),
        "the person's file is not written"
    );
}

/// dnf5's overrides decide: a section its file switches off and an override
/// switches on counts as on, and the reverse as off.
#[test]
fn an_override_decides_whether_the_section_is_on() {
    let m = Machine::fedora();
    m.f.file(
        NVIDIA_REPO_PATH,
        NVIDIA_REPO.replacen("enabled=1", "enabled=0", 1),
    );
    m.f.file(
        "/etc/dnf/repos.override.d/99-config_manager.repo",
        "[nvidia-container-toolkit]\nenabled=1\n",
    );
    assert_eq!(kinds(&m.plan()), ["s2_add_key", "s4_install"]);

    let m = Machine::fedora();
    m.nvidia_section();
    m.f.file(
        "/etc/dnf/repos.override.d/99-config_manager.repo",
        "[nvidia-container-toolkit]\nenabled=0\n",
    );
    assert_eq!(
        kinds(&m.plan()),
        ["s1_enable_repo", "s2_add_key", "s4_install"]
    );
}

/// A `.repo` file only root can read is skipped (root's helper skips it too,
/// so both plans agree) and named.
#[test]
fn an_unreadable_repo_file_is_skipped_and_named() {
    let m = Machine::fedora();
    m.f.file_mode(NVIDIA_REPO_PATH, NVIDIA_REPO, 0o600);
    let inputs = m.inputs();
    assert!(inputs
        .repos
        .skipped
        .iter()
        .any(|s| s.path.to_str() == Some(NVIDIA_REPO_PATH)));
    assert_eq!(
        kinds(&m.plan()),
        ["s1_add_repo", "s2_add_key", "s4_install"]
    );
}

/// A file at S1's or S2's path that lmgw did not write is never
/// overwritten: the plan has no steps and says which file is in the way.
#[test]
fn a_file_of_lmgws_names_that_lmgw_did_not_write_is_never_overwritten() {
    for (path, content) in [
        (
            REPO_FILE,
            "[my-own]\nname=mine\nbaseurl=https://example.org/repo\n",
        ),
        (
            KEY_FILE,
            "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nAAAA\n-----END PGP PUBLIC KEY BLOCK-----\n",
        ),
    ] {
        let m = Machine::fedora();
        m.f.file(path, content);
        let p = m.plan();
        assert!(p.steps.is_empty(), "{path}: {:?}", kinds(&p));
        assert_eq!(p.blocked.len(), 1, "{path}");
        let why = p.blocked[0].to_string();
        assert!(
            why.contains(path) && why.contains("never overwrites"),
            "{why}"
        );
    }
    // Owned by a user, or a symbolic link: not lmgw's either.
    let m = Machine::fedora();
    m.own_repo(REPO_TEXT);
    m.f.chown(REPO_FILE, 1000);
    assert!(matches!(m.inputs().repo_file, OwnFile::NotOwn(_)));
    let m = Machine::fedora();
    m.f.file("/home/someone/x.repo", REPO_TEXT)
        .link(REPO_FILE, "/home/someone/x.repo");
    assert!(matches!(m.inputs().repo_file, OwnFile::NotOwn(why) if why.contains("symbolic link")));
}

/// lmgw's own file with an older lmgw's text is replaced (through a
/// temporary file, which the helper does): S1 with `replace`.
#[test]
fn lmgws_own_file_with_another_text_is_replaced() {
    let m = Machine::fedora();
    let older = format!("{REPO_MARKER} An older text.\n[lmgw-nvidia-container-toolkit]\nname=old\nbaseurl=https://nvidia.github.io/libnvidia-container/stable/rpm/$basearch\nenabled=1\n");
    m.own_repo(&older);
    let p = m.plan();
    assert_eq!(kinds(&p), ["s1_add_repo", "s2_add_key", "s4_install"]);
    let w = p.steps[0].file().unwrap();
    assert!(w.replace);
    assert_eq!(w.content, REPO_TEXT);
    assert_eq!(p.section.unwrap().origin, Origin::Lmgw);

    // Kept current with no toolkit step due, too.
    m.nvidia_toolkit().trust_nvidia();
    m.f.file(KEY_FILE, lmgw_machine::plan::embedded::NVIDIA_KEY);
    let p = m.plan();
    assert_eq!(kinds(&p), ["s1_add_repo"]);
    assert!(matches!(&p.steps[0], Step::AddRepo { write } if write.replace));

    // Current: nothing.
    m.own_repo(REPO_TEXT);
    assert!(m.plan().steps.is_empty());
}

/// lmgw's own section in place, its key written and trusted: the install
/// alone.
#[test]
fn with_lmgws_section_and_key_in_place_only_the_install_runs() {
    let m = Machine::fedora();
    m.own_repo(REPO_TEXT).trust_nvidia();
    m.f.file(KEY_FILE, lmgw_machine::plan::embedded::NVIDIA_KEY);
    assert_eq!(kinds(&m.plan()), ["s4_install"]);
}

/// S2's condition from `rpmkeys --list`: the key file written either way,
/// the import only while rpm does not list NVIDIA's fingerprint; a
/// `rpmkeys` that fails blocks the plan rather than guessing.
#[test]
fn the_key_is_imported_only_while_rpm_does_not_trust_it() {
    let m = Machine::fedora();
    let p = m.plan();
    assert_eq!(p.steps[1].commands().len(), 1, "imported");
    m.trust_nvidia();
    let p = m.plan();
    assert_eq!(kinds(&p), ["s1_add_repo", "s2_add_key", "s4_install"]);
    assert!(p.steps[1].commands().is_empty(), "not imported again");
    assert!(p.steps[1].file().is_some());
    m.rpm.lock().unwrap().keys_fail = true;
    let p = m.plan();
    assert!(p.steps.is_empty());
    assert!(
        p.blocked[0].to_string().contains("trusted keys"),
        "{}",
        p.blocked[0]
    );
}

/// The provider table, when it names an enabled repository of this system:
/// no NVIDIA repository and no key (S1, S2 left out), the install scoped to
/// that repository and checked against the distribution's own keys.
#[test]
fn a_provider_table_row_installs_from_the_distribution() {
    const TABLE: &[Provider] = &[Provider {
        os_id: "fedora",
        version_id: "44",
        repo_id: "fedora",
        package: "nvidia-container-toolkit",
    }];
    let m = Machine::fedora();
    let p = m.plan_with(Want::default(), TABLE);
    assert_eq!(kinds(&p), ["s4_install"]);
    assert_eq!(
        p.steps[0].commands(),
        [[
            "/usr/bin/dnf5",
            "install",
            "-y",
            "--repo=fedora",
            "--setopt=fedora.skip_if_unavailable=False",
            "--setopt=install_weak_deps=False",
            "nvidia-container-toolkit"
        ]]
    );
    assert_eq!(p.section.unwrap().origin, Origin::Provider);
    // Another system: the row does not apply.
    m.f.file("/etc/os-release", "ID=fedora\nVERSION_ID=45\n");
    assert_eq!(
        kinds(&m.plan_with(Want::default(), TABLE)),
        ["s1_add_repo", "s2_add_key", "s4_install"]
    );
    // The production table is empty today.
    assert!(PROVIDERS.is_empty());
}
