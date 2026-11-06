//! `apply` (§3.1, §6 "The helper"): who may start it, the hash check against
//! root's own plans, the running unit, and the system task it starts — the
//! unit around the inhibitor around `run` — with every exit code it maps.

use std::io;

use lmgw_machine::helper::{unit_argv, Event, Exit, Stream, UNIT_SERVICE};
use lmgw_machine::plan::PLAN_SCHEMA;
use lmgw_machine_helper::{Caller, Ended, VERSION};

use super::support::{
    helper, runs_begun, runs_lines_for, through_pkexec, unit_lines, FakeHost, Machine,
};

#[test]
fn apply_refuses_without_pkexec_and_root() {
    let m = Machine::fedora();
    let h = m.hash(false);
    let read_before = m.f.calls().len();
    let cases = [
        Caller {
            pkexec_uid: None,
            ..through_pkexec()
        },
        Caller {
            pkexec_uid: Some("".into()),
            ..through_pkexec()
        },
        Caller {
            pkexec_uid: Some("1000; rm".into()),
            ..through_pkexec()
        },
        // Digits, but past a 32-bit user ID; and a sign.
        Caller {
            pkexec_uid: Some("4294967296".into()),
            ..through_pkexec()
        },
        Caller {
            pkexec_uid: Some("+1000".into()),
            ..through_pkexec()
        },
        // A user who is not root.
        Caller {
            ruid: 1000,
            euid: 1000,
            ..through_pkexec()
        },
        // Root's effective ID only, as a setuid copy would have it.
        Caller {
            ruid: 1000,
            euid: 0,
            ..through_pkexec()
        },
    ];
    for caller in cases {
        let host = FakeHost::new();
        let o = helper(&["apply", &h], &caller, &m, &host);
        assert_eq!(o.exit, Exit::Refused, "{caller:?}");
        assert_eq!(o.names(), ["refused"], "{caller:?}");
        assert!(host.untouched(), "{caller:?}");
        // Nothing in the journal: it is root's record of what root did, and
        // anyone can start the program (D82).
        assert!(host.journal().is_empty(), "{caller:?}");
    }
    // Nothing of the machine was read for a refused caller.
    assert_eq!(m.f.calls().len(), read_before, "{:?}", m.f.calls());
}

/// A hash that is none of root's plans: exit 2, nothing started, and the
/// line says root's plans with its schema and version, so the app can tell
/// "the computer changed" from "lmgw was updated".
#[test]
fn a_hash_root_does_not_plan_changes_nothing() {
    let m = Machine::fedora();
    let host = FakeHost::new();
    let other = "f".repeat(64);
    let o = helper(&["apply", &other], &through_pkexec(), &m, &host);
    assert_eq!(o.exit, Exit::PlanChanged);
    let Event::PlanChanged {
        asked,
        schema,
        version,
        plans,
    } = &o.events[1]
    else {
        panic!("{:?}", o.events)
    };
    assert_eq!(asked, &other);
    assert_eq!(*schema, PLAN_SCHEMA);
    assert_eq!(version, VERSION);
    let hashes: Vec<&str> = plans.iter().map(|p| p.sha256.as_str()).collect();
    assert_eq!(hashes, [m.hash(false), m.hash(true)]);
    assert!(host.untouched());
    assert!(m.f.calls_of("/usr/bin/systemctl").is_empty());
}

/// The fresh-Fedora plan: the unit is started exactly as `unit_argv` says
/// (systemd-run around systemd-inhibit around `helper run <hash>`), its
/// lines are passed on, its stderr becomes notes, and its exit is `apply`'s.
#[test]
fn an_accepted_plan_starts_the_unit_around_the_inhibitor() {
    let m = Machine::fedora();
    let h = m.hash(false);
    let host = FakeHost::new();
    let hh = h.clone();
    host.answer(move |_| {
        let mut lines = unit_lines(&hh, 0);
        lines.push((
            Stream::Stderr,
            "Running as unit: lmgw-machine-setup.service".into(),
        ));
        (lines, Ok(Ended::exited(0)))
    });
    let o = helper(&["apply", &h], &through_pkexec(), &m, &host);
    assert_eq!(o.exit, Exit::Done);
    assert_eq!(host.runs(), [unit_argv(&h)]);
    let argv = &host.runs()[0];
    assert_eq!(argv[0], "/usr/bin/systemd-run");
    assert!(argv.contains(&"--unit=lmgw-machine-setup".to_string()));
    let inhibit = argv
        .iter()
        .position(|a| a == "/usr/bin/systemd-inhibit")
        .unwrap();
    assert!(argv[inhibit..].contains(&"--what=shutdown:sleep".to_string()));
    assert_eq!(
        &argv[argv.len() - 3..],
        ["/usr/libexec/lmgw/lmgw-machine-helper", "run", h.as_str()]
    );
    assert_eq!(
        o.names(),
        ["begun", "begun", "started", "finished", "note"],
        "{:?}",
        o.events
    );
    assert!(matches!(&o.events[4], Event::Note { line } if line.contains("Running as unit")));
    // apply's own begun names the user pkexec says asked, in its line and
    // in its journal entry; run's has none.
    assert!(
        matches!(&o.events[0], Event::Begun { verb, pkexec_uid: Some(1000), .. } if verb == "apply"),
        "{:?}",
        o.events[0]
    );
    assert!(matches!(
        &o.events[1],
        Event::Begun {
            pkexec_uid: None,
            ..
        }
    ));
    let (_, message, line) = &host.journal()[0];
    assert!(
        message.ends_with("asked for by user 1000 through pkexec"),
        "{message}"
    );
    assert!(line.contains(r#""pkexec_uid":1000"#), "{line}");
    // Nothing written by apply itself; it asked systemd first.
    assert!(host.writes().is_empty());
    assert_eq!(
        m.f.calls_of("/usr/bin/systemctl"),
        [["show", "--property=ActiveState", "--value", UNIT_SERVICE]]
    );
}

/// The plan with the toolkit update (S4′), offered when lmgw's own spec
/// generation failed, is accepted as the other of root's two (D44).
#[test]
fn the_toolkit_update_variant_is_accepted() {
    let m = Machine::fedora().with_nvidia_toolkit();
    let h = m.hash(true);
    assert_ne!(h, m.hash(false));
    let host = FakeHost::new();
    let hh = h.clone();
    host.answer(move |_| (unit_lines(&hh, 0), Ok(Ended::exited(0))));
    let o = helper(&["apply", &h], &through_pkexec(), &m, &host);
    assert_eq!(o.exit, Exit::Done, "{:?}", o.events);
    assert_eq!(host.runs(), [unit_argv(&h)]);
}

/// A plan without steps (NVIDIA's toolkit working, no update asked) starts
/// no unit.
#[test]
fn a_plan_without_steps_starts_nothing() {
    let m = Machine::fedora().with_nvidia_toolkit();
    let host = FakeHost::new();
    let o = helper(&["apply", &m.hash(false)], &through_pkexec(), &m, &host);
    assert_eq!(o.exit, Exit::Done);
    assert_eq!(o.names(), ["begun", "nothing_to_do"]);
    assert!(host.untouched());
}

#[test]
fn a_running_unit_gives_exit_5_and_starts_nothing() {
    for state in ["active", "activating", "deactivating"] {
        let m = Machine::fedora();
        m.set_unit_state(Some(state));
        let host = FakeHost::new();
        let o = helper(&["apply", &m.hash(false)], &through_pkexec(), &m, &host);
        assert_eq!(o.exit, Exit::AlreadyRunning, "{state}");
        assert!(
            matches!(&o.events[1], Event::AlreadyRunning { state: s } if s == state),
            "{:?}",
            o.events
        );
        assert!(host.untouched());
    }
}

/// `run`'s own exit codes come back through systemd-run and the inhibitor,
/// each with the line of `run`'s that says it.
#[test]
fn runs_exit_codes_come_back_through_the_unit() {
    for (code, exit) in [
        (0, Exit::Done),
        (2, Exit::PlanChanged),
        (3, Exit::StepFailed),
        (4, Exit::Refused),
    ] {
        let m = Machine::fedora();
        let h = m.hash(false);
        let host = FakeHost::new();
        let hh = h.clone();
        host.answer(move |_| (runs_lines_for(&hh, code), Ok(Ended::exited(code))));
        let o = helper(&["apply", &h], &through_pkexec(), &m, &host);
        assert_eq!(o.exit, exit, "{code}: {:?}", o.events);
    }
    // `nothing_to_do` from the unit is a 0 of run's own as well.
    let m = Machine::fedora();
    let h = m.hash(false);
    let host = FakeHost::new();
    host.answer(|_| {
        (
            vec![runs_begun(), (Stream::Stdout, Event::NothingToDo.to_line())],
            Ok(Ended::exited(0)),
        )
    });
    let o = helper(&["apply", &h], &through_pkexec(), &m, &host);
    assert_eq!(o.exit, Exit::Done, "{:?}", o.events);
}

/// An exit code that `run`'s own last line does not say is not passed on:
/// it may be systemd-run's, the inhibitor's or anything else the unit ran.
/// Exit 6, with what `run` said.
#[test]
fn an_exit_code_runs_line_does_not_say_is_exit_6() {
    let finished = |exit| (Stream::Stdout, Event::Finished { exit }.to_line());
    type Case = (i32, Vec<(Stream, String)>, &'static str);
    let cases: Vec<Case> = vec![
        // The steps began, and the unit ended 0 without run's last line.
        (
            0,
            unit_lines(&"0".repeat(64), 0)[..2].to_vec(),
            "said nothing",
        ),
        (0, vec![runs_begun()], "said nothing"),
        (0, vec![runs_begun(), finished(3)], "finished (exit 3)"),
        (2, vec![runs_begun(), finished(0)], "finished (exit 0)"),
        (3, vec![runs_begun(), finished(0)], "finished (exit 0)"),
        (3, Vec::new(), "said nothing"),
        (4, Vec::new(), "said nothing"),
        (2, Vec::new(), "said nothing"),
    ];
    for (code, lines, said) in cases {
        let m = Machine::fedora();
        let h = m.hash(false);
        let host = FakeHost::new();
        let lines = std::sync::Mutex::new(Some(lines));
        host.answer(move |_| {
            (
                lines.lock().unwrap().take().unwrap(),
                Ok(Ended::exited(code)),
            )
        });
        let o = helper(&["apply", &h], &through_pkexec(), &m, &host);
        assert_eq!(o.exit, Exit::TaskFailed, "{code} {said}: {:?}", o.events);
        let Some(Event::TaskFailed { why }) = o.events.last() else {
            panic!("{code} {said}: {:?}", o.events)
        };
        assert!(why.contains(&format!("exit {code}")), "{why}");
        assert!(why.contains(said), "{said}: {why}");
    }
}

/// systemd refused the unit's name because another start began it between
/// apply's question and its start: exit 5, as if it had asked a moment
/// later.
#[test]
fn a_unit_started_meanwhile_gives_exit_5() {
    let m = Machine::fedora();
    let h = m.hash(false);
    let host = FakeHost::new();
    let answers = m.answers.clone();
    host.answer(move |_| {
        answers.lock().unwrap().unit_state = Some("active".into());
        (
            vec![(
                Stream::Stderr,
                "Failed to start transient service unit: Unit lmgw-machine-setup.service was \
                 already loaded or has a fragment file."
                    .into(),
            )],
            Ok(Ended::exited(1)),
        )
    });
    let o = helper(&["apply", &h], &through_pkexec(), &m, &host);
    assert_eq!(o.exit, Exit::AlreadyRunning, "{:?}", o.events);
    assert_eq!(o.names(), ["begun", "note", "already_running"]);
}

/// The unit could not be started, ended by a signal, or systemd-run could
/// not be run at all: exit 6 with what was seen.
#[test]
fn a_task_that_does_not_say_how_it_went_is_exit_6() {
    let cases: Vec<(&str, io::Result<Ended>, bool)> = vec![
        ("exit 1", Ok(Ended::exited(1)), false),
        (
            "signal 15",
            Ok(Ended {
                code: None,
                signal: Some(15),
            }),
            true,
        ),
        // The inhibitor not found: the unit's main process failed to exec.
        ("exit 203", Ok(Ended::exited(203)), false),
        (
            "could not be started",
            Err(io::Error::from(io::ErrorKind::NotFound)),
            false,
        ),
    ];
    for (said, ended, started) in cases {
        let m = Machine::fedora();
        let h = m.hash(false);
        let host = FakeHost::new();
        let hh = h.clone();
        let ended = std::sync::Mutex::new(Some(ended));
        host.answer(move |_| {
            let lines = if started {
                unit_lines(&hh, 0)[..2].to_vec()
            } else {
                Vec::new()
            };
            (lines, ended.lock().unwrap().take().unwrap())
        });
        let o = helper(&["apply", &h], &through_pkexec(), &m, &host);
        assert_eq!(o.exit, Exit::TaskFailed, "{said}");
        let Some(Event::TaskFailed { why }) = o.events.last() else {
            panic!("{said}: {:?}", o.events)
        };
        assert!(why.contains(said), "{said}: {why}");
        assert!(host.journal().iter().any(|(p, _, _)| *p == 3), "{said}");
    }
}

/// lmgw's pipe carries events only: a line on the unit's stdout that is not
/// one is passed on as a note.
#[test]
fn a_unit_line_that_is_not_an_event_becomes_a_note() {
    let m = Machine::fedora();
    let h = m.hash(false);
    let host = FakeHost::new();
    let hh = h.clone();
    host.answer(move |_| {
        let mut lines = vec![(Stream::Stdout, "plain words".to_string())];
        lines.extend(unit_lines(&hh, 0));
        (lines, Ok(Ended::exited(0)))
    });
    let o = helper(&["apply", &h], &through_pkexec(), &m, &host);
    assert_eq!(o.exit, Exit::Done, "{:?}", o.events);
    assert!(
        matches!(&o.events[1], Event::Note { line } if line.ends_with("said: plain words")),
        "{:?}",
        o.events
    );
}

/// systemctl that cannot say whether the unit runs does not stop apply:
/// systemd refuses a second unit of the name anyway, and a note says so.
#[test]
fn a_failing_systemctl_is_said_and_apply_goes_on() {
    let m = Machine::fedora();
    m.set_unit_state(None);
    let h = m.hash(false);
    let host = FakeHost::new();
    let hh = h.clone();
    host.answer(move |_| (unit_lines(&hh, 0), Ok(Ended::exited(0))));
    let o = helper(&["apply", &h], &through_pkexec(), &m, &host);
    assert_eq!(o.exit, Exit::Done);
    assert!(
        matches!(&o.events[1], Event::Note { line } if line.contains("Failed to connect to bus")),
        "{:?}",
        o.events
    );
    assert_eq!(host.runs().len(), 1);
}

/// An image-based system's plan is shown as commands to type (§3.7); the
/// helper never runs its rpm-ostree install, even for its own hash.
#[test]
fn an_image_based_plan_is_refused() {
    let m = Machine::fedora().ostree();
    let p = m.plan(false);
    assert!(!p.steps.is_empty());
    let host = FakeHost::new();
    let o = helper(&["apply", &p.hash()], &through_pkexec(), &m, &host);
    assert_eq!(o.exit, Exit::Refused);
    assert!(
        matches!(&o.events[1], Event::Refused { why } if why.contains("image-based")),
        "{:?}",
        o.events
    );
    assert!(host.untouched());
}
