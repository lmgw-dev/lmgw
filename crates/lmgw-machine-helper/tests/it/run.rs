//! `run` (§3.1, §6 "The helper"): only inside the system task, the hash
//! checked again, then each step exactly as the plan states it, stopping at
//! the first failure, every line on stdout and in the journal, a closed
//! stdout never stopping a step.

use std::io;

use lmgw_machine::helper::{Event, Exit, Stream};
use lmgw_machine::plan::embedded::{KEY_FILE, NVIDIA_KEY, REPO_FILE, REPO_TEXT};
use lmgw_machine::plan::StepKind;
use lmgw_machine_helper::{Caller, Ended};

use super::support::{
    helper, helper_into, in_the_unit, through_pkexec, ClosedPipe, FakeHost, Machine,
};

#[test]
fn run_refuses_outside_the_unit() {
    let m = Machine::fedora();
    let h = m.hash(false);
    let read_before = m.f.calls().len();
    let cases = [
        // Started by pkexec directly: lmgw's scope, not the unit.
        through_pkexec(),
        Caller {
            ruid: 1000,
            euid: 1000,
            ..in_the_unit()
        },
        Caller {
            ruid: 1000,
            ..in_the_unit()
        },
        Caller {
            cgroup: Ok("0::/system.slice/lmgw-machine-setup.service.d\n".into()),
            ..in_the_unit()
        },
        // cgroup v1 only: no 0:: line.
        Caller {
            cgroup: Ok("1:name=systemd:/system.slice/lmgw-machine-setup.service\n".into()),
            ..in_the_unit()
        },
        Caller {
            cgroup: Err("No such file or directory (os error 2)".into()),
            ..in_the_unit()
        },
    ];
    for caller in cases {
        let host = FakeHost::new();
        let o = helper(&["run", &h], &caller, &m, &host);
        assert_eq!(o.exit, Exit::Refused, "{caller:?}");
        assert_eq!(o.names(), ["refused"], "{caller:?}");
        assert!(host.untouched(), "{caller:?}");
        assert!(host.journal().is_empty(), "{caller:?}");
    }
    // Nothing of the machine was read for a refused caller.
    assert_eq!(m.f.calls().len(), read_before, "{:?}", m.f.calls());
}

/// The machine changed between `apply` and the unit starting: exit 2,
/// nothing run, nothing written.
#[test]
fn run_checks_the_hash_again() {
    let m = Machine::fedora();
    let h = m.hash(false);
    // NVIDIA's key became trusted meanwhile: S2 no longer imports it.
    m.answers.lock().unwrap().trusted = true;
    let host = FakeHost::new();
    let o = helper(&["run", &h], &in_the_unit(), &m, &host);
    assert_eq!(o.exit, Exit::PlanChanged);
    assert_eq!(o.names(), ["begun", "plan_changed"]);
    assert!(host.untouched());
}

/// The fresh-Fedora plan: lmgw's repository file and NVIDIA's key created
/// (never replacing), then each command exactly as the plan states it, in
/// order, every event journaled.
#[test]
fn each_step_runs_exactly_as_the_plan_states_it() {
    let m = Machine::fedora();
    let plan = m.plan(false);
    let host = FakeHost::new();
    host.answer(|argv| {
        if argv[0] == "/usr/bin/dnf5" {
            (
                vec![
                    (
                        Stream::Stdout,
                        "Installing: nvidia-container-toolkit-base".into(),
                    ),
                    (Stream::Stderr, "warning: a note from dnf".into()),
                ],
                Ok(Ended::exited(0)),
            )
        } else {
            (Vec::new(), Ok(Ended::exited(0)))
        }
    });
    let o = helper(&["run", &plan.hash()], &in_the_unit(), &m, &host);
    assert_eq!(o.exit, Exit::Done, "{:?}", o.events);

    let want: Vec<Vec<String>> = plan.steps.iter().flat_map(|s| s.commands()).collect();
    assert_eq!(host.runs(), want);
    assert_eq!(host.runs().len(), 2, "rpmkeys --import, dnf5 install");
    assert_eq!(
        host.runs()[0],
        [
            "/usr/bin/rpmkeys",
            "--import",
            "/etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit"
        ]
    );
    assert_eq!(&host.runs()[1][..3], ["/usr/bin/dnf5", "install", "-y"]);
    assert_eq!(
        host.writes(),
        [
            (REPO_FILE.to_string(), false, REPO_TEXT.to_string()),
            (KEY_FILE.to_string(), false, NVIDIA_KEY.to_string()),
        ]
    );

    assert_eq!(o.names()[..3], ["begun", "started", "step_started"]);
    assert_eq!(o.names().last().unwrap(), "finished");
    assert!(matches!(&o.events[1], Event::Started { steps: 3, .. }));
    let ended: Vec<(usize, StepKind, Option<i32>)> = o
        .events
        .iter()
        .filter_map(|e| match e {
            Event::StepEnded {
                step, kind, exit, ..
            } => Some((*step, *kind, *exit)),
            _ => None,
        })
        .collect();
    assert_eq!(
        ended,
        [
            (1, StepKind::AddRepo, Some(0)),
            (2, StepKind::AddKey, Some(0)),
            (3, StepKind::Install, Some(0)),
        ]
    );
    // dnf's lines, each on its own stream, under its step.
    assert!(o.events.contains(&Event::Line {
        step: 3,
        kind: StepKind::Install,
        stream: Stream::Stderr,
        line: "warning: a note from dnf".into(),
    }));
    // The helper says each command before it starts it.
    assert!(o.events.iter().any(|e| matches!(e,
        Event::Line { step: 3, stream: Stream::Helper, line, .. }
            if line.starts_with("running /usr/bin/dnf5 install -y --repo="))));
    // Every event is in the journal too, its JSON line as a field.
    let journal = host.journal();
    assert_eq!(journal.len(), o.events.len());
    for ((_, _, line), e) in journal.iter().zip(&o.events) {
        assert_eq!(Event::parse(line).as_ref(), Some(e));
    }
    assert!(journal
        .iter()
        .any(|(_, msg, _)| msg == "step 3 (s4_install) stderr: warning: a note from dnf"));
}

/// S4′ through the person's NVIDIA section, as root plans it.
#[test]
fn the_toolkit_update_runs_its_scoped_upgrade() {
    let m = Machine::fedora().with_nvidia_toolkit();
    let plan = m.plan(true);
    let host = FakeHost::new();
    let o = helper(&["run", &plan.hash()], &in_the_unit(), &m, &host);
    assert_eq!(o.exit, Exit::Done, "{:?}", o.events);
    let upgrade = &host.runs()[0];
    assert_eq!(upgrade, &plan.steps[1].commands()[0]);
    assert_eq!(&upgrade[..3], ["/usr/bin/dnf5", "upgrade", "-y"]);
    assert!(upgrade.contains(&"--repo=nvidia-container-toolkit".to_string()));
    assert_eq!(upgrade.last().unwrap(), "nvidia-container-toolkit-base");
    // lmgw's key file, which `<scope>`'s gpgkey names, written first.
    assert_eq!(host.writes()[0].0, KEY_FILE);
}

/// Every step kind of the closed list, over the machines that need them:
/// each program run exactly as the plan's argv says, in order, each file
/// written as the plan's file says, and nothing else.
#[test]
fn every_step_kind_runs_exactly_as_the_plan_states_it() {
    let machines = [
        ("fresh", Machine::fedora(), false),
        (
            "switched off",
            Machine::fedora().with_switched_off_section(),
            false,
        ),
        ("retired", Machine::fedora().with_retired_toolkit(), false),
        ("update", Machine::fedora().with_nvidia_toolkit(), true),
    ];
    let mut kinds = Vec::new();
    for (name, m, update) in machines {
        let plan = m.plan(update);
        assert!(plan.blocked.is_empty(), "{name}: {:?}", plan.blocked);
        let host = FakeHost::new();
        let o = helper(&["run", &plan.hash()], &in_the_unit(), &m, &host);
        assert_eq!(o.exit, Exit::Done, "{name}: {:?}", o.events);
        let argv: Vec<Vec<String>> = plan.steps.iter().flat_map(|s| s.commands()).collect();
        assert_eq!(host.runs(), argv, "{name}");
        let files: Vec<(String, bool, String)> = plan
            .steps
            .iter()
            .filter_map(|s| s.file())
            .map(|w| (w.path.to_string(), w.replace, w.content.to_string()))
            .collect();
        assert_eq!(host.writes(), files, "{name}");
        kinds.extend(plan.steps.iter().map(|s| s.kind()));
    }
    for kind in [
        StepKind::AddRepo,
        StepKind::EnableRepo,
        StepKind::AddKey,
        StepKind::Install,
        StepKind::Update,
        StepKind::Swap,
    ] {
        assert!(kinds.contains(&kind), "{kind:?} not covered: {kinds:?}");
    }
}

/// A failing step stops the rest: its output and its exit code are said,
/// the steps after it never run.
#[test]
fn a_failing_step_stops_the_rest() {
    let m = Machine::fedora();
    let plan = m.plan(false);
    let host = FakeHost::new();
    host.answer(|argv| {
        if argv[0] == "/usr/bin/rpmkeys" {
            (
                vec![(
                    Stream::Stderr,
                    "error: cannot open Packages database".into(),
                )],
                Ok(Ended::exited(1)),
            )
        } else {
            (Vec::new(), Ok(Ended::exited(0)))
        }
    });
    let o = helper(&["run", &plan.hash()], &in_the_unit(), &m, &host);
    assert_eq!(o.exit, Exit::StepFailed);
    assert_eq!(host.runs().len(), 1, "dnf5 never ran: {:?}", host.runs());
    assert!(o.events.contains(&Event::Line {
        step: 2,
        kind: StepKind::AddKey,
        stream: Stream::Stderr,
        line: "error: cannot open Packages database".into(),
    }));
    assert!(o.events.contains(&Event::StepEnded {
        step: 2,
        kind: StepKind::AddKey,
        exit: Some(1),
        signal: None,
        error: None,
    }));
    assert_eq!(o.events.last(), Some(&Event::Finished { exit: 3 }));
    assert!(!o
        .events
        .iter()
        .any(|e| matches!(e, Event::StepStarted { step: 3, .. })));
}

/// A step killed by a signal, or whose program cannot be started, fails too.
#[test]
fn a_signal_or_a_program_that_cannot_start_fails_the_step() {
    for (ended, want) in [
        (
            Ok(Ended {
                code: None,
                signal: Some(9),
            }),
            (None, Some(9), false),
        ),
        (
            Err(io::Error::from(io::ErrorKind::NotFound)),
            (None, None, true),
        ),
    ] {
        let m = Machine::fedora();
        let plan = m.plan(false);
        let host = FakeHost::new();
        let ended = std::sync::Mutex::new(Some(ended));
        host.answer(move |_| (Vec::new(), ended.lock().unwrap().take().unwrap()));
        let o = helper(&["run", &plan.hash()], &in_the_unit(), &m, &host);
        assert_eq!(o.exit, Exit::StepFailed);
        let Some(Event::StepEnded {
            exit,
            signal,
            error,
            ..
        }) = o
            .events
            .iter()
            .rev()
            .find(|e| matches!(e, Event::StepEnded { .. }))
        else {
            panic!("{:?}", o.events)
        };
        assert_eq!((*exit, *signal, error.is_some()), want);
        if let Some(e) = error {
            assert!(
                e.starts_with("/usr/bin/rpmkeys could not be started"),
                "{e}"
            );
        }
    }
}

/// A file that cannot be written fails its step before its commands run.
#[test]
fn a_file_that_cannot_be_written_fails_its_step() {
    let m = Machine::fedora();
    let plan = m.plan(false);
    let host = FakeHost::new();
    *host.write_fails.lock().unwrap() = Some(KEY_FILE.to_string());
    let o = helper(&["run", &plan.hash()], &in_the_unit(), &m, &host);
    assert_eq!(o.exit, Exit::StepFailed);
    assert!(host.runs().is_empty(), "rpmkeys never ran");
    let Some(Event::StepEnded {
        step: 2,
        error: Some(e),
        ..
    }) = o
        .events
        .iter()
        .find(|e| matches!(e, Event::StepEnded { step: 2, .. }))
    else {
        panic!("{:?}", o.events)
    };
    assert!(
        e.contains(KEY_FILE) && e.contains("Permission denied"),
        "{e}"
    );
}

/// lmgw quit while the steps run: every write to the pipe fails, and the
/// steps go on to their end, with the journal complete.
#[test]
fn a_closed_stdout_does_not_stop_a_step() {
    let m = Machine::fedora();
    let plan = m.plan(false);
    let host = FakeHost::new();
    let o = helper_into(
        &["run", &plan.hash()],
        &in_the_unit(),
        &m,
        &host,
        &mut ClosedPipe,
    );
    assert_eq!(o.exit, Exit::Done);
    assert_eq!(host.runs().len(), 2);
    assert_eq!(host.writes().len(), 2);
    assert!(host
        .journal()
        .iter()
        .any(|(_, msg, _)| msg == "finished: every step ran"));
}

/// A journal that takes nothing is said once on stdout; the steps go on.
#[test]
fn a_journal_that_takes_nothing_is_said_once() {
    let m = Machine::fedora();
    let plan = m.plan(false);
    let host = FakeHost::new();
    *host.journal_fails.lock().unwrap() = true;
    let o = helper(&["run", &plan.hash()], &in_the_unit(), &m, &host);
    assert_eq!(o.exit, Exit::Done);
    let notes = o
        .events
        .iter()
        .filter(
            |e| matches!(e, Event::Note { line } if line.starts_with("the journal did not take")),
        )
        .count();
    assert_eq!(notes, 1);
}

/// `run` refuses an image-based plan too, should the unit ever be started
/// with one.
#[test]
fn run_refuses_an_image_based_plan() {
    let m = Machine::fedora().ostree();
    let host = FakeHost::new();
    let o = helper(&["run", &m.plan(false).hash()], &in_the_unit(), &m, &host);
    assert_eq!(o.exit, Exit::Refused);
    assert!(host.untouched());
}
