//! What every generated plan looks like, over a grid of machines (§6, "The
//! plan"): each dnf command that installs, updates or swaps ends with a
//! package name and is scoped with `--repo=`; none is a bare `dnf upgrade
//! -y`; every program is an absolute path; the steps come in S1/S1′, S2, S4
//! order.

use lmgw_machine::plan::embedded::{KEY_FILE, NVIDIA_KEY, REPO_TEXT};
use lmgw_machine::plan::{PackageName, Plan, Want, PROVIDERS};

use super::machine::{owner_line, Machine, NVIDIA_REPO, NVIDIA_REPO_PATH};

fn owners() -> Vec<Option<Option<String>>> {
    vec![
        // No file.
        None,
        // A file nobody owns.
        Some(None),
        Some(Some(owner_line(
            "nvidia-container-toolkit-base",
            "1.20.1",
            Some("ddcae044f796ecb0"),
        ))),
        Some(Some(owner_line(
            "nvidia-container-toolkit-base",
            "1.20.1",
            Some("0123456789abcdef"),
        ))),
        Some(Some(owner_line(
            "golang-github-nvidia-container-toolkit",
            "1.17.4",
            Some("829b606631645531"),
        ))),
    ]
}

/// Every plan of the grid: toolkit owner × NVIDIA section (none, on, off) ×
/// lmgw's own files (none, current, older) × key trusted × full toolkit ×
/// the update flag.
fn every_plan() -> Vec<(String, Plan)> {
    let mut out = Vec::new();
    for owner in owners() {
        for section in 0..3 {
            for own in 0..3 {
                for trusted in [false, true] {
                    for full in [false, true] {
                        for update_toolkit in [false, true] {
                            let m = Machine::fedora();
                            if let Some(line) = &owner {
                                m.toolkit(line.clone());
                            }
                            match section {
                                1 => {
                                    m.nvidia_section();
                                }
                                2 => {
                                    m.f.file(
                                        NVIDIA_REPO_PATH,
                                        NVIDIA_REPO.replacen("enabled=1", "enabled=0", 1),
                                    );
                                }
                                _ => {}
                            }
                            match own {
                                1 => {
                                    m.own_repo(REPO_TEXT);
                                    m.f.file(KEY_FILE, NVIDIA_KEY);
                                }
                                2 => {
                                    m.own_repo("# Written by lmgw. Older.\n[lmgw-nvidia-container-toolkit]\nbaseurl=https://nvidia.github.io/libnvidia-container/stable/rpm/$basearch\n");
                                }
                                _ => {}
                            }
                            if trusted {
                                m.trust_nvidia();
                            }
                            m.rpm.lock().unwrap().full_toolkit = full;
                            let label = format!(
                                "owner {owner:?} section {section} own {own} trusted {trusted} \
                                 full {full} update {update_toolkit}"
                            );
                            out.push((label, m.plan_with(Want { update_toolkit }, PROVIDERS)));
                        }
                    }
                }
            }
        }
    }
    out
}

#[test]
fn every_dnf_step_names_its_packages_last_and_is_scoped() {
    let plans = every_plan();
    let mut dnf_steps = 0;
    for (label, plan) in &plans {
        for step in &plan.steps {
            for argv in step.commands() {
                assert!(argv[0].starts_with('/'), "{label}: {argv:?}");
                if argv[0] != "/usr/bin/dnf5" || argv[1] == "config-manager" {
                    continue;
                }
                dnf_steps += 1;
                assert!(
                    matches!(argv[1].as_str(), "install" | "upgrade" | "swap"),
                    "{label}: {argv:?}"
                );
                assert_ne!(argv.len(), 3, "{label}: a bare {argv:?}");
                let packages = step.packages();
                assert!(!packages.is_empty(), "{label}: {argv:?}");
                let tail = &argv[argv.len() - packages.len()..];
                assert_eq!(tail, packages.as_slice(), "{label}: {argv:?}");
                for p in tail {
                    assert!(PackageName::new(p).is_some(), "{label}: {p}");
                    assert!(!p.starts_with('-'), "{label}: {p}");
                }
                assert!(
                    argv.iter().any(|a| a.starts_with("--repo=")),
                    "{label}: unscoped {argv:?}"
                );
            }
        }
        // S1 or S1′ before S2 before the toolkit step.
        let order: Vec<u8> = plan
            .steps
            .iter()
            .map(|s| match s.kind().as_str() {
                "s1_add_repo" | "s1_enable_repo" => 1,
                "s2_add_key" => 2,
                _ => 4,
            })
            .collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted, "{label}");
        // A blocked plan runs nothing.
        if !plan.blocked.is_empty() {
            assert!(plan.steps.is_empty(), "{label}");
        }
    }
    assert!(dnf_steps > 50, "the grid exercised {dnf_steps} dnf steps");
}

/// A name that would be an option to dnf is never a package or section name.
#[test]
fn a_name_that_is_an_option_is_no_name() {
    for bad in ["-y", "--repo=x", "", ".hidden", "a b", "a;rm"] {
        assert!(PackageName::new(bad).is_none(), "{bad:?}");
    }
    assert!(PackageName::new("nvidia-container-toolkit-base").is_some());
    assert!(lmgw_machine::plan::RepoId::new("--x").is_none());
    assert!(lmgw_machine::plan::RepoId::new("copr:copr.fedorainfracloud.org:a:b").is_some());
    // dnf5 matches the id part of a setopt as a glob: none may get in.
    for glob in ["nvidia-*", "a?b", "a[bc]"] {
        assert!(lmgw_machine::plan::RepoId::new(glob).is_none(), "{glob:?}");
    }
}
