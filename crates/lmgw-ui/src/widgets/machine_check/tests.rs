//! The plan block's text, which mirrors the shell's own confirmation.

use lmgw_api_types::machine::{MachinePlan, PlanFile, PlanHow, PlanStep, PlanStepKind};

use super::items::Mark;
use super::plan::{
    command_line, elsewhere, root_lines, terminal_text, type_them, ELSEWHERE, OWN_WINDOW,
};
use super::this_computer;
use lmgw_api_types::machine::ItemState;

fn plan() -> MachinePlan {
    MachinePlan {
        hash: "0".repeat(64),
        how: PlanHow::Helper,
        steps: vec![
            PlanStep {
                kind: PlanStepKind::S2AddKey,
                sentence: "Add NVIDIA's signing key".into(),
                commands: vec![vec![
                    "/usr/bin/rpmkeys".into(),
                    "--import".into(),
                    "/etc/pki/rpm-gpg/RPM-GPG-KEY-example".into(),
                ]],
                writes_file: Some(PlanFile {
                    path: "/etc/pki/rpm-gpg/RPM-GPG-KEY-example".into(),
                    content: "-----BEGIN PGP PUBLIC KEY BLOCK-----\nexample\n".into(),
                    mode: 0o644,
                    replaces: false,
                    key_fingerprint: Some("AAAA BBBB".into()),
                }),
                download_bytes: None,
                terminal: vec!["sudo rpmkeys --import /etc/pki/rpm-gpg/RPM-GPG-KEY-example".into()],
                package_scripts: None,
            },
            PlanStep {
                kind: PlanStepKind::S4Install,
                sentence: "Install NVIDIA's tool · 8.1 MiB download".into(),
                commands: vec![vec![
                    "/usr/bin/dnf5".into(),
                    "install".into(),
                    "-y".into(),
                    "it's here".into(),
                ]],
                writes_file: None,
                download_bytes: Some(8_500_000),
                terminal: vec!["sudo dnf5 install -y example-base".into()],
                package_scripts: Some("NVIDIA's package then runs its own install scripts.".into()),
            },
        ],
    }
}

#[test]
fn commands_are_quoted_as_a_terminal_reads_them() {
    assert_eq!(
        command_line(&["/usr/bin/dnf5".into(), "--setopt=a.b=False".into()]),
        "/usr/bin/dnf5 --setopt=a.b=False"
    );
    assert_eq!(
        command_line(&["echo".into(), "it's".into(), String::new()]),
        r"echo 'it'\''s' ''"
    );
}

#[test]
fn the_fold_lists_what_root_runs_as_the_dialog_does() {
    let lines = root_lines(&plan());
    // Only the package's own scripts are a note, not a command.
    assert_eq!(
        lines.iter().map(|l| l.note).collect::<Vec<_>>(),
        [false, false, false, true]
    );
    assert_eq!(
        lines.into_iter().map(|l| l.text).collect::<Vec<_>>(),
        vec![
            "create /etc/pki/rpm-gpg/RPM-GPG-KEY-example (mode 0644)   (the file below: an \
             OpenPGP key, fingerprint AAAA BBBB)",
            "/usr/bin/rpmkeys --import /etc/pki/rpm-gpg/RPM-GPG-KEY-example",
            "/usr/bin/dnf5 install -y 'it'\\''s here'",
            "NVIDIA's package then runs its own install scripts.",
        ]
    );
}

#[test]
fn the_copy_button_copies_every_steps_terminal_lines() {
    assert_eq!(
        terminal_text(&plan().steps),
        "sudo rpmkeys --import /etc/pki/rpm-gpg/RPM-GPG-KEY-example\nsudo dnf5 install -y \
         example-base"
    );
}

#[test]
fn a_browser_is_told_where_lmgw_runs() {
    assert_eq!(this_computer(true), "this computer");
    assert_eq!(this_computer(false), "the computer lmgw runs on");
    // On the same computer the app's own window is the way; elsewhere,
    // the computer lmgw runs on (review M3).
    assert_eq!(elsewhere(true), OWN_WINDOW);
    assert_eq!(elsewhere(false), ELSEWHERE);
    assert_eq!(type_them(true, true), "Type these in a terminal:");
    assert_eq!(
        type_them(false, false),
        "To type them in a terminal yourself on the computer lmgw runs on:"
    );
}

#[test]
fn with_no_card_the_deciding_item_reads_as_information() {
    assert_eq!(Mark::item(ItemState::Problem, true), Mark::Info);
    assert_eq!(Mark::item(ItemState::Problem, false), Mark::Problem);
    assert_eq!(Mark::item(ItemState::NotChecked, true), Mark::NotChecked);
}
