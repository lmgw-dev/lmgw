//! The confirmation (§4.5, D25, D133, D134, D140): "Not now" starts
//! nothing, the dialog's text and fold, the window it needs in front, a
//! "Not now" that holds for its document, and "Set it up" waiting with the
//! focus.

use super::*;

/// "Not now" starts nothing; the dialog showed the shell's own plan.
#[tokio::test]
async fn the_confirmation_declined_starts_nothing() {
    let w = Fake {
        confirm: false,
        ..Fake::new()
    };
    let (r, events) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Declined);
    assert_eq!(r.sentence, "Nothing was changed.");
    assert_eq!((w.asked(), w.spawned()), (1, 0));
    assert!(events.is_empty(), "{events:?}");
}

/// The dialog's text: the bullets, what NVIDIA's package does besides, the
/// password line, and behind the fold every command by its absolute path,
/// each file with its mode (a key with its fingerprint), what the package's
/// own scripts change, and every file in full.
#[test]
fn the_confirmation_shows_the_steps_the_password_and_the_commands() {
    let c = crate::machine::confirm::of(&plan_with_key());
    assert_eq!(c.title, "Set up your graphics card for local models");
    assert!(c
        .text
        .contains("• Add NVIDIA's software source (nvidia.github.io)\n• Add NVIDIA's signing key"));
    assert!(c.text.contains(
        "Besides these, NVIDIA's package runs its own install scripts; \"Show the commands\" says \
         what they change."
    ));
    assert!(c
        .text
        .contains("Your computer will ask for your password once."));
    assert!(
        !c.text.contains("only these things"),
        "the package's scripts change more: {}",
        c.text
    );
    assert!(!c.text.contains("dnf5"), "commands only behind the fold");
    let lines: Vec<&str> = c.commands.lines().collect();
    assert_eq!(
        lines[..6],
        [
            "These run as the administrator (root), in this order, as the system task \
             lmgw-machine-setup:",
            "",
            "create /etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo (mode 0644)   (the file \
             below)",
            "create /etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit (mode 0644)   (the \
             file below: an OpenPGP key, fingerprint C95B 321B 61E8 8C18 09C4 F759 DDCA E044 F796 \
             ECB0)",
            "/usr/bin/rpmkeys --import /etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit",
            "/usr/bin/dnf5 install -y --repo=lmgw-nvidia-container-toolkit \
             nvidia-container-toolkit-base",
        ]
    );
    assert_eq!(lines[6], SCRIPTS, "the package's note right after its step");
    assert!(c.commands.contains(&format!(
        "\n\n/etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo, in full (mode 0644):\n{}\n\n",
        REPO_TEXT.trim_end()
    )));
    assert!(c.commands.ends_with(
        "/etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit, in full (mode 0644):\n\
         -----BEGIN PGP PUBLIC KEY BLOCK-----\n\nmQINBF…"
    ));
    assert_eq!(
        (
            crate::machine::confirm::NOT_NOW,
            crate::machine::confirm::SET_IT_UP
        ),
        ("Not now", "Set it up")
    );

    // A plan that installs no package changes only what it lists.
    let mut p = plan(HASH, PlanHow::Helper);
    p.steps.truncate(1);
    let c = crate::machine::confirm::of(&p);
    assert!(
        c.text.ends_with("lmgw changes only these things."),
        "{}",
        c.text
    );
    assert!(!c.text.contains("Besides these"));
}

/// The dialog shows only over the app's window in front (D133): hidden to
/// the tray, behind another program, or with no document, the answer is
/// to go to the window, and nothing is asked or started.
#[tokio::test]
async fn the_confirmation_needs_the_window_in_front() {
    let cases = [
        (
            Window {
                visible: false,
                ..in_front(1)
            },
            "hidden",
        ),
        (
            Window {
                active: false,
                ..in_front(1)
            },
            "not the active one",
        ),
        (
            Window {
                document: None,
                ..in_front(1)
            },
            "no document",
        ),
    ];
    for (window, why) in cases {
        let w = Fake::new().windows(&[window]);
        let (r, events) = run(&w).await;
        assert_eq!(r.outcome, MachineApplyOutcome::Unavailable, "{r:#?}");
        assert_eq!(r.sentence, "Open lmgw's window and press Set it up there.");
        assert!(
            r.detail.iter().any(|d| d.contains(why)),
            "{why}: {:?}",
            r.detail
        );
        assert!(r.check.is_some(), "the fresh check is the answer's");
        assert_eq!((w.asked(), w.spawned()), (0, 0));
        assert!(events.is_empty());
    }
}

/// A "Not now" holds for the document that said it (D140): its next call
/// gets the same answer without a dialog, and a new document is asked
/// again.
#[tokio::test]
async fn a_not_now_holds_until_the_window_runs_another_document() {
    let w = Fake {
        confirm: false,
        ..Fake::new().windows(&[in_front(7), in_front(7), in_front(8)])
    };
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Declined);
    assert_eq!(r.sentence, "Nothing was changed.");
    assert_eq!(w.asked(), 1);

    let (r, events) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Declined, "{r:#?}");
    assert_eq!(
        r.sentence,
        "Nothing was changed: you chose \"Not now\" already. Reload the page to be asked again."
    );
    assert_eq!(w.asked(), 1, "no second dialog in the same document");
    assert!(events.is_empty());

    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Declined);
    assert_eq!(r.sentence, "Nothing was changed.");
    assert_eq!(w.asked(), 2, "a new document is asked again");
    assert_eq!(w.spawned(), 0);

    // "Set it up" in the next document goes on, and the memory is clear.
    let asked = Asked::new();
    assert_eq!(asked.answered(3, false), Answer::NotNow);
    assert_eq!(asked.may_show(in_front(3)), Err(Answer::DeclinedBefore));
    assert_eq!(asked.may_show(in_front(4)), Ok(4));
    assert_eq!(asked.answered(4, true), Answer::SetItUp);
    assert_eq!(asked.may_show(in_front(3)), Ok(3));
}

/// "Set it up" waits a second with the dialog mapped and active, and again
/// after every time it lost the focus (D134); a timer from before a change
/// arms nothing.
#[test]
fn set_it_up_waits_a_second_with_the_focus() {
    assert_eq!(ARM_DELAY, std::time::Duration::from_secs(1));
    let mut a = Arming::default();
    assert!(!a.armed());
    assert_eq!(a.mapped(true), None, "mapped, not yet active");
    let t1 = a.active(true).expect("a wait begins");
    assert!(!a.armed());
    assert!(a.elapsed(t1));
    assert_eq!(a.active(true), None, "no change, no new wait");
    assert!(a.armed());

    // The focus goes elsewhere: disarmed at once, and its old timer is
    // spent.
    assert_eq!(a.active(false), None);
    assert!(!a.armed());
    assert!(!a.elapsed(t1));
    let t2 = a.active(true).unwrap();
    assert_ne!(t1, t2);
    assert!(!a.elapsed(t1), "a stale timer");
    assert!(a.elapsed(t2));

    // Unmapped (minimized, hidden): disarmed; mapped again: wait again.
    assert_eq!(a.mapped(false), None);
    assert!(!a.armed());
    assert!(!a.elapsed(t2));
    let t3 = a.mapped(true).unwrap();
    assert!(a.elapsed(t3));

    // Focus lost and back inside one wait: only the last wait counts.
    let mut a = Arming::default();
    a.mapped(true);
    let t1 = a.active(true).unwrap();
    a.active(false);
    let t2 = a.active(true).unwrap();
    assert!(!a.elapsed(t1));
    assert!(a.elapsed(t2));
}
