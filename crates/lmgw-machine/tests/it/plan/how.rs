//! The button or instructions (§3.7, §3.4): which systems get which, the
//! same commands either way, and the helper's installation as the shell
//! will check it.

use lmgw_machine::plan::embedded::{HELPER, PKEXEC, POLICY};
use lmgw_machine::plan::instructions::helper_installed;
use lmgw_machine::plan::{how, terminal_lines, How, UserSide};

use super::machine::Machine;

fn user(helper: Result<(), String>) -> UserSide {
    UserSide {
        appimage: false,
        helper,
        other_provider: None,
        read_differently: Vec::new(),
    }
}

fn why(h: How) -> String {
    match h {
        How::Instructions { why } => why,
        How::Helper => panic!("the button, not instructions"),
    }
}

/// The helper installed as §3.4 requires.
fn install_helper(m: &Machine) {
    m.f.file_mode(HELPER, b"\x7fELF", 0o755)
        .file_mode(POLICY, b"<policyconfig/>", 0o644)
        .file_mode(PKEXEC, b"\x7fELF", 0o4755);
}

#[test]
fn fedora_with_the_helper_gets_the_button() {
    let m = Machine::fedora();
    install_helper(&m);
    let helper = helper_installed(&m.f);
    assert_eq!(helper, Ok(()));
    assert_eq!(how(&m.plan(), &user(helper)), How::Helper);
}

/// A plan the helper read differently (D88) gets instructions saying so;
/// another plan on the same machine still gets the button.
#[test]
fn a_plan_root_read_differently_gets_instructions() {
    let m = Machine::fedora();
    install_helper(&m);
    let plan = m.plan();
    let read_differently = UserSide {
        read_differently: vec![plan.hash()],
        ..user(Ok(()))
    };
    assert_eq!(
        why(how(&plan, &read_differently)),
        lmgw_machine::plan::instructions::READ_DIFFERENTLY
    );
    let another = UserSide {
        read_differently: vec!["0".repeat(64)],
        ..user(Ok(()))
    };
    assert_eq!(how(&plan, &another), How::Helper);
}

/// The confirmation's command lines: the program by its absolute path, each
/// argument quoted only where a shell would read it.
#[test]
fn a_command_line_quotes_what_a_shell_would_read() {
    let argv: Vec<String> = ["/usr/bin/dnf5", "install", "-y", "a b", "it's", "$HOME"]
        .map(String::from)
        .to_vec();
    assert_eq!(
        lmgw_machine::plan::instructions::command_line(&argv),
        r#"/usr/bin/dnf5 install -y 'a b' 'it'\''s' '$HOME'"#
    );
}

/// Without the helper (today's builds), on an AppImage, beside another
/// repository that offers the toolkit, on an image-based system and without
/// dnf: instructions, each saying why, with the same steps.
#[test]
fn every_other_system_gets_instructions_saying_why() {
    let m = Machine::fedora();
    let plan = m.plan();
    let w = why(how(&plan, &user(helper_installed(&m.f))));
    assert_eq!(
        w,
        "This version of lmgw can't do this by itself yet, so here are the commands to run in a \
         terminal."
    );

    install_helper(&m);
    let appimage = UserSide {
        appimage: true,
        ..user(Ok(()))
    };
    let w = why(how(&plan, &appimage));
    assert!(w.contains("AppImage") && w.contains("RPM"), "{w}");
    // The same plan: the instructions type what the helper would run.
    assert_eq!(m.plan().hash(), plan.hash());

    let other = UserSide {
        other_provider: Some("fedora".into()),
        ..user(Ok(()))
    };
    let w = why(how(&plan, &other));
    assert!(w.contains("(fedora)") && w.contains("the command"), "{w}");

    m.f.file("/run/ostree-booted", b"");
    let w = why(how(&m.plan(), &user(Ok(()))));
    assert!(w.contains("read-only image"), "{w}");
    m.f.remove("/run/ostree-booted");

    m.f.remove("/usr/bin/dnf5").remove("/usr/bin/dnf");
    let w = why(how(&m.plan(), &user(Ok(()))));
    assert!(w.contains("install-guide"), "{w}");
}

/// A plan with no steps never says "here are the commands": it says what is
/// in the way and what the person can do instead (§3.7).
#[test]
fn a_plan_without_steps_promises_no_commands() {
    let promises = |w: &str| w.contains("here are the commands") || w.contains("Here's how");
    // Blocked: a file of lmgw's name that is the person's.
    let m = Machine::fedora();
    install_helper(&m);
    m.f.file(
        lmgw_machine::plan::embedded::REPO_FILE,
        "[mine]\nbaseurl=https://example.org/\n",
    );
    let p = m.plan();
    assert!(p.steps.is_empty());
    let w = why(how(&p, &user(Ok(()))));
    assert!(!promises(&w), "{w}");
    assert!(w.contains("remove it, then check again"), "{w}");

    // An older dnf: lmgw shows none.
    let m = Machine::fedora();
    m.f.remove("/usr/bin/dnf5").remove("/usr/bin/dnf");
    m.f.file_mode("/usr/bin/dnf", b"#!/usr/bin/python3\n", 0o755);
    let p = m.plan();
    assert!(p.steps.is_empty());
    let w = why(how(&p, &user(Ok(()))));
    assert!(!promises(&w) && w.contains("shows none here"), "{w}");

    // An update on a read-only image: no step of lmgw's; the system's own
    // update brings it.
    let m = Machine::fedora();
    m.nvidia_toolkit().nvidia_section().trust_nvidia();
    m.f.file("/run/ostree-booted", b"");
    let p = m.plan_with(
        lmgw_machine::plan::Want {
            update_toolkit: true,
        },
        lmgw_machine::plan::PROVIDERS,
    );
    assert!(p.steps.is_empty(), "{:?}", super::machine::kinds(&p));
    let w = why(how(&p, &user(Ok(()))));
    assert!(!promises(&w) && w.contains("rpm-ostree upgrade"), "{w}");

    // The retired package on a read-only image.
    let m = Machine::fedora();
    m.toolkit(Some(super::machine::owner_line(
        "golang-github-nvidia-container-toolkit",
        "1.17.4",
        Some("829b606631645531"),
    )));
    m.f.file("/run/ostree-booted", b"");
    let p = m.plan();
    assert!(p.steps.is_empty());
    let w = why(how(&p, &user(Ok(()))));
    assert!(!promises(&w) && w.contains("install-guide"), "{w}");
}

/// Another repository offering the toolkit: one install from it alone, with
/// the system's own signature checks, and nothing of NVIDIA's added.
#[test]
fn the_toolkit_from_another_repository_is_one_scoped_install() {
    let m = Machine::fedora();
    let p =
        lmgw_machine::plan::plan_from_other(&m.inputs(), "fedora", "nvidia-container-toolkit-base");
    assert_eq!(super::machine::kinds(&p), ["s4_install"]);
    assert_eq!(
        p.steps[0].commands(),
        [[
            "/usr/bin/dnf5",
            "install",
            "-y",
            "--repo=fedora",
            "--setopt=fedora.skip_if_unavailable=False",
            "--setopt=install_weak_deps=False",
            "nvidia-container-toolkit-base"
        ]]
    );
    assert_eq!(
        terminal_lines(&p.steps[0]),
        [
            "sudo dnf5 install -y --repo=fedora --setopt=fedora.skip_if_unavailable=False \
          --setopt=install_weak_deps=False nvidia-container-toolkit-base"
        ]
    );
    let other = UserSide {
        other_provider: Some("fedora".into()),
        ..user(Ok(()))
    };
    assert!(why(how(&p, &other)).contains("(fedora)"));
    // Root's plan never has this step: its hash is not one root accepts.
    assert!(
        !lmgw_machine::plan::accepted_hashes(&m.inputs(), lmgw_machine::plan::PROVIDERS)
            .contains(&p.hash())
    );
    // An id or name that would be an option plans nothing.
    let p =
        lmgw_machine::plan::plan_from_other(&m.inputs(), "--x", "nvidia-container-toolkit-base");
    assert!(p.steps.is_empty() && !p.blocked.is_empty());
}

/// A plan with a file in the way is instructions naming the file.
#[test]
fn a_blocked_plan_names_what_is_in_the_way() {
    let m = Machine::fedora();
    install_helper(&m);
    m.f.file(
        lmgw_machine::plan::embedded::REPO_FILE,
        "[mine]\nbaseurl=https://example.org/\n",
    );
    let w = why(how(&m.plan(), &user(Ok(()))));
    assert!(w.contains("lmgw-nvidia-container-toolkit.repo"), "{w}");
}

/// The commands to type: files as quoted here-documents through `sudo tee`
/// (so `$basearch` stays literal), then each argv with `sudo` and the
/// program's plain name; rpm-ostree's install ends with the restart.
#[test]
fn the_terminal_lines_are_the_steps_with_sudo() {
    let m = Machine::fedora();
    let p = m.plan();
    let s1 = terminal_lines(&p.steps[0]);
    assert_eq!(
        s1[0],
        "sudo tee /etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo > /dev/null <<'EOF'"
    );
    assert!(s1.iter().any(|l| l.ends_with("rpm/$basearch")));
    assert_eq!(s1.last().unwrap(), "EOF");
    let s2 = terminal_lines(&p.steps[1]);
    assert_eq!(
        s2.last().unwrap(),
        "sudo rpmkeys --import /etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit"
    );
    let s4 = terminal_lines(&p.steps[2]);
    assert_eq!(s4.len(), 1);
    assert!(s4[0].starts_with("sudo dnf5 install -y --repo=lmgw-nvidia-container-toolkit "));
    assert!(s4[0].ends_with(" nvidia-container-toolkit-base"));

    m.f.file("/run/ostree-booted", b"");
    let p = m.plan();
    let s4 = terminal_lines(&p.steps[2]);
    assert_eq!(
        s4,
        [
            "sudo rpm-ostree install nvidia-container-toolkit-base",
            "systemctl reboot"
        ]
    );
}

/// §3.4's installation rules for the helper, its action and pkexec.
#[test]
fn the_helper_counts_as_installed_only_when_root_owns_it_safely() {
    let check = |setup: &dyn Fn(&Machine)| {
        let m = Machine::fedora();
        install_helper(&m);
        setup(&m);
        helper_installed(&m.f)
    };
    assert_eq!(check(&|_| {}), Ok(()));
    type Setup<'a> = &'a dyn Fn(&Machine);
    let cases: [(&str, Setup); 7] = [
        ("not there", &|m| {
            m.f.remove(HELPER);
        }),
        ("mode 0775", &|m| {
            m.f.file_mode(HELPER, b"", 0o775);
        }),
        ("setuid", &|m| {
            m.f.file_mode(HELPER, b"", 0o4755);
        }),
        ("belongs to user", &|m| {
            m.f.chown(HELPER, 1000);
        }),
        ("symbolic link", &|m| {
            m.f.file("/opt/x", b"").link(HELPER, "/opt/x");
        }),
        ("polkit action", &|m| {
            m.f.remove(POLICY);
        }),
        ("/usr/bin/pkexec", &|m| {
            m.f.remove(PKEXEC);
        }),
    ];
    for (expect, setup) in cases {
        let e = check(setup).unwrap_err();
        assert!(e.contains(expect), "{expect}: {e}");
    }
    // A directory on its way others can write.
    let e = check(&|m| {
        m.f.chown("/usr/libexec/lmgw", 1000);
    })
    .unwrap_err();
    assert!(e.contains("/usr/libexec/lmgw"), "{e}");
}
