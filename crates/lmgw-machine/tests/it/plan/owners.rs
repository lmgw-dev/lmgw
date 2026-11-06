//! The toolkit by owner (§1.2's owner table, D24): each row gives its plan.

use lmgw_machine::plan::embedded::{KEY_FILE, NVIDIA_KEY, REPO_FILE, REPO_TEXT};
use lmgw_machine::plan::{
    accepted_hashes, plan, Installer, Origin, Step, Toolkit, Want, PROVIDERS,
};

use super::machine::{kinds, owner_line, Machine};

const SCOPE_OWN: [&str; 5] = [
    "--repo=lmgw-nvidia-container-toolkit",
    "--setopt=lmgw-nvidia-container-toolkit.skip_if_unavailable=False",
    "--setopt=lmgw-nvidia-container-toolkit.gpgcheck=1",
    "--setopt=lmgw-nvidia-container-toolkit.gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit",
    "--setopt=install_weak_deps=False",
];

/// No toolkit on a fresh Fedora: lmgw's own section (S1), NVIDIA's key (S2,
/// written and imported) and the install (S4) — the commands of §4.5's fold,
/// to the letter.
#[test]
fn no_toolkit_gets_lmgws_section_nvidias_key_and_the_install() {
    let m = Machine::fedora();
    let p = m.plan();
    assert_eq!(p.toolkit, Toolkit::Absent);
    assert!(p.blocked.is_empty(), "{:?}", p.blocked);
    assert_eq!(kinds(&p), ["s1_add_repo", "s2_add_key", "s4_install"]);
    let s1 = p.steps[0].file().unwrap();
    assert_eq!(
        (s1.path, s1.content, s1.mode, s1.replace),
        (REPO_FILE, REPO_TEXT, 0o644, false)
    );
    assert!(p.steps[0].commands().is_empty());
    let s2 = p.steps[1].file().unwrap();
    assert_eq!(
        (s2.path, s2.content, s2.replace),
        (KEY_FILE, NVIDIA_KEY, false)
    );
    assert_eq!(
        p.steps[1].commands(),
        [["/usr/bin/rpmkeys", "--import", KEY_FILE]]
    );
    let mut install = vec!["/usr/bin/dnf5", "install", "-y"];
    install.extend(SCOPE_OWN);
    install.push("nvidia-container-toolkit-base");
    assert_eq!(p.steps[2].commands(), [install]);
    let section = p.section.unwrap();
    assert_eq!(
        (section.id.as_str(), section.origin),
        ("lmgw-nvidia-container-toolkit", Origin::Lmgw)
    );
}

/// NVIDIA's own package, signed by NVIDIA's key: nothing to do while lmgw's
/// spec generates; S4′ once it does not.
#[test]
fn nvidias_toolkit_needs_nothing_until_generation_fails() {
    let m = Machine::fedora();
    m.nvidia_toolkit().nvidia_section().trust_nvidia();
    let p = m.plan();
    assert!(matches!(p.toolkit, Toolkit::Nvidia(_)));
    assert!(p.steps.is_empty(), "{:?}", kinds(&p));

    let p = m.plan_with(
        Want {
            update_toolkit: true,
        },
        PROVIDERS,
    );
    // The person's section is used as it is; only lmgw's key file is
    // written, since rpm trusts the key already.
    assert_eq!(kinds(&p), ["s2_add_key", "s4_update"]);
    assert!(p.steps[0].commands().is_empty(), "no import: rpm trusts it");
    let argv = &p.steps[1].commands()[0];
    assert_eq!(&argv[..3], ["/usr/bin/dnf5", "upgrade", "-y"]);
    assert!(argv.contains(&"--repo=nvidia-container-toolkit".to_string()));
    assert!(argv.contains(&"--setopt=nvidia-container-toolkit.gpgcheck=1".to_string()));
    assert_eq!(argv.last().unwrap(), "nvidia-container-toolkit-base");
    assert_eq!(p.section.unwrap().origin, Origin::Person);
}

/// With NVIDIA's full toolkit installed, which pins `-base`'s exact
/// version, S4′ takes both: the owner package first. The person's section
/// lets every package through, so nothing is appended to its filter.
#[test]
fn an_update_takes_the_full_toolkit_along_when_it_is_installed() {
    let m = Machine::fedora();
    m.nvidia_toolkit().nvidia_section().trust_nvidia();
    m.rpm.lock().unwrap().full_toolkit = true;
    let p = m.plan_with(
        Want {
            update_toolkit: true,
        },
        PROVIDERS,
    );
    let Step::Update { packages, .. } = &p.steps[1] else {
        panic!("{:?}", kinds(&p))
    };
    let names: Vec<&str> = packages.iter().map(|p| p.as_str()).collect();
    assert_eq!(
        names,
        ["nvidia-container-toolkit-base", "nvidia-container-toolkit"]
    );
    let argv = &p.steps[1].commands()[0];
    assert_eq!(
        &argv[argv.len() - 2..],
        ["nvidia-container-toolkit-base", "nvidia-container-toolkit"]
    );
    assert!(!argv.iter().any(|a| a.contains("includepkgs")), "{argv:?}");
}

/// Through lmgw's own section, whose file lets only `-base` through, S4′
/// with the whole family installed updates all four together and appends
/// the other three to the section's filter for that one command: otherwise
/// dnf could not update `-base` past the full toolkit's exact pin.
#[test]
fn an_update_through_lmgws_section_lets_the_installed_family_through() {
    use lmgw_machine::plan::embedded::{KEY_FILE, NVIDIA_KEY, REPO_TEXT};
    let m = Machine::fedora();
    m.nvidia_toolkit().own_repo(REPO_TEXT).trust_nvidia();
    m.f.file(KEY_FILE, NVIDIA_KEY);
    {
        let mut r = m.rpm.lock().unwrap();
        r.full_toolkit = true;
        r.libraries = true;
    }
    let p = m.plan_with(
        Want {
            update_toolkit: true,
        },
        PROVIDERS,
    );
    assert_eq!(kinds(&p), ["s4_update"]);
    assert_eq!(p.section.as_ref().unwrap().origin, Origin::Lmgw);
    let argv = &p.steps[0].commands()[0];
    assert_eq!(
        &argv[argv.len() - 4..],
        [
            "nvidia-container-toolkit-base",
            "nvidia-container-toolkit",
            "libnvidia-container-tools",
            "libnvidia-container1"
        ]
    );
    let appended: Vec<&str> = argv
        .iter()
        .filter_map(|a| a.strip_prefix("--setopt=lmgw-nvidia-container-toolkit.includepkgs="))
        .collect();
    assert_eq!(
        appended,
        [
            "nvidia-container-toolkit",
            "libnvidia-container-tools",
            "libnvidia-container1"
        ]
    );
    // Only the update widens it: an install through the same section does not.
    let m = Machine::fedora();
    m.own_repo(REPO_TEXT).trust_nvidia();
    m.f.file(KEY_FILE, NVIDIA_KEY);
    let p = m.plan();
    assert_eq!(kinds(&p), ["s4_install"]);
    assert!(!p.steps[0].commands()[0]
        .iter()
        .any(|a| a.contains("includepkgs")));
}

/// A person's section whose own filter keeps out an installed package that
/// has to move with the rest blocks the update with that reason, instead of
/// a command dnf would refuse.
#[test]
fn a_persons_section_that_filters_out_the_family_blocks_the_update() {
    let m = Machine::fedora();
    m.nvidia_toolkit().trust_nvidia();
    m.f.file(
        super::machine::NVIDIA_REPO_PATH,
        super::machine::NVIDIA_REPO.replacen(
            "enabled=1",
            "enabled=1\nincludepkgs=nvidia-container-toolkit-base",
            1,
        ),
    );
    m.rpm.lock().unwrap().full_toolkit = true;
    let p = m.plan_with(
        Want {
            update_toolkit: true,
        },
        PROVIDERS,
    );
    assert!(p.steps.is_empty(), "{:?}", kinds(&p));
    assert_eq!(
        p.blocked,
        [lmgw_machine::plan::Blocker::NotOffered {
            section: "nvidia-container-toolkit".into(),
            package: "nvidia-container-toolkit".into()
        }]
    );
}

/// NVIDIA's package name signed by another key, or by none, is not taken for
/// NVIDIA's: it is someone else's, used as it is and never replaced.
#[test]
fn an_owner_signed_by_another_key_is_not_nvidias() {
    for key in [Some("0123456789abcdef"), None] {
        let m = Machine::fedora();
        m.toolkit(Some(owner_line(
            "nvidia-container-toolkit-base",
            "1.20.1",
            key,
        )))
        .nvidia_section();
        let p = m.plan_with(
            Want {
                update_toolkit: true,
            },
            PROVIDERS,
        );
        assert!(matches!(&p.toolkit, Toolkit::Other(_)), "{:?}", p.toolkit);
        assert!(p.steps.is_empty(), "{key:?}: {:?}", kinds(&p));
    }
}

/// Fedora's retired Go package: S4″ swaps it for NVIDIA's, through lmgw's
/// own section on a machine without NVIDIA's.
#[test]
fn fedoras_retired_package_is_swapped_for_nvidias() {
    let m = Machine::fedora();
    m.toolkit(Some(owner_line(
        "golang-github-nvidia-container-toolkit",
        "1.17.4",
        Some("829b606631645531"),
    )));
    let p = m.plan();
    assert!(matches!(&p.toolkit, Toolkit::Retired(p) if p.version == "1.17.4"));
    assert_eq!(kinds(&p), ["s1_add_repo", "s2_add_key", "s4_swap"]);
    let mut swap = vec!["/usr/bin/dnf5", "swap", "-y"];
    swap.extend(SCOPE_OWN);
    swap.extend([
        "golang-github-nvidia-container-toolkit",
        "nvidia-container-toolkit-base",
    ]);
    assert_eq!(p.steps[2].commands(), [swap]);
    assert!(p.steps[2]
        .sentence()
        .contains("golang-github-nvidia-container-toolkit"));
}

/// A file nobody owns (put there by hand) and any other owner are used as
/// they are: never in an install, update or swap step.
#[test]
fn a_file_nobody_owns_or_another_package_is_never_installed_over() {
    for line in [
        None,
        Some(owner_line(
            "my-own-toolkit",
            "0.1",
            Some("0123456789abcdef"),
        )),
    ] {
        let m = Machine::fedora();
        m.toolkit(line.clone());
        for update_toolkit in [false, true] {
            let p = m.plan_with(Want { update_toolkit }, PROVIDERS);
            assert!(
                matches!(p.toolkit, Toolkit::NotOwned | Toolkit::Other(_)),
                "{:?}",
                p.toolkit
            );
            assert!(p.steps.is_empty(), "{line:?}: {:?}", kinds(&p));
        }
    }
}

/// rpm that cannot answer: no steps, and the plan says why.
#[test]
fn an_rpm_that_cannot_answer_plans_nothing_and_says_so() {
    let m = Machine::fedora();
    m.f.file_mode("/usr/bin/nvidia-ctk", b"", 0o755);
    m.f.program("/usr/bin/rpm", |_| {
        Ok(lmgw_machine::CmdOut::failure(
            2,
            "error: rpmdb open failed\n",
        ))
    });
    let p = m.plan();
    assert!(matches!(p.toolkit, Toolkit::Unknown(_)));
    assert!(p.steps.is_empty());
    assert_eq!(p.blocked.len(), 1);
    assert!(
        p.blocked[0].to_string().contains("rpmdb open failed"),
        "{}",
        p.blocked[0]
    );
}

/// Root cannot know whether lmgw's spec generation failed, so it accepts
/// the plan for either answer: here, nothing, or S4′.
#[test]
fn root_accepts_the_plan_with_and_without_the_update() {
    let m = Machine::fedora();
    m.nvidia_toolkit().nvidia_section().trust_nvidia();
    let inputs = m.inputs();
    let accepted = accepted_hashes(&inputs, PROVIDERS);
    assert_eq!(accepted.len(), 2);
    for update_toolkit in [false, true] {
        let user = plan(&inputs, Want { update_toolkit }, PROVIDERS).hash();
        assert!(accepted.contains(&user));
    }
    // Where the flag changes nothing, one hash.
    let fresh = Machine::fedora();
    assert_eq!(accepted_hashes(&fresh.inputs(), PROVIDERS).len(), 1);
}

/// On an image-based system the install is rpm-ostree's, after lmgw's
/// repository file and key file; nothing is imported into the image's rpm.
#[test]
fn an_image_based_system_installs_with_rpm_ostree() {
    let m = Machine::fedora();
    m.f.file("/run/ostree-booted", b"");
    let p = m.plan();
    assert_eq!(kinds(&p), ["s1_add_repo", "s2_add_key", "s4_install"]);
    assert!(p.steps[1].commands().is_empty());
    assert!(matches!(
        &p.steps[2],
        Step::Install { via: Installer::RpmOstree, package } if package.as_str() == "nvidia-container-toolkit-base"
    ));
    assert_eq!(
        p.steps[2].commands(),
        [[
            "/usr/bin/rpm-ostree",
            "install",
            "nvidia-container-toolkit-base"
        ]]
    );
}

/// Without dnf, or with a dnf older than dnf5, lmgw has no commands of its
/// own: the plan is empty and the instructions point at NVIDIA's guide.
#[test]
fn no_dnf5_plans_no_commands() {
    let m = Machine::fedora();
    m.f.remove("/usr/bin/dnf5").remove("/usr/bin/dnf");
    m.f.file_mode("/usr/bin/dnf", b"#!/usr/bin/python3\n", 0o755);
    assert_eq!(m.inputs().system(), lmgw_machine::plan::System::OtherDnf);
    assert!(m.plan().steps.is_empty(), "an older dnf");
    m.f.remove("/usr/bin/dnf");
    assert_eq!(m.inputs().system(), lmgw_machine::plan::System::NoDnf);
    let p = m.plan();
    assert!(p.steps.is_empty());
    assert!(p.needs_toolkit());
}
