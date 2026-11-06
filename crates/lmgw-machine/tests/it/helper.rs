//! What the root helper and the app say to each other (machine-check design
//! §3.1, §3.5, WP6): the exit codes, the JSON lines both parse, the system
//! task's command, and root's plans per flag value.

use lmgw_machine::helper::{
    is_plan_hash, unit_argv, unit_running, variants, Event, Exit, PlanVariant, Stream,
};
use lmgw_machine::plan::{accepted_hashes, Inputs, StepKind, PLAN_SCHEMA, PROVIDERS};

use super::plan::machine::Machine;

#[test]
fn exit_codes_stay_clear_of_pkexecs_and_read_back() {
    let all = [
        (Exit::Done, 0),
        (Exit::PlanChanged, 2),
        (Exit::StepFailed, 3),
        (Exit::Refused, 4),
        (Exit::AlreadyRunning, 5),
        (Exit::TaskFailed, 6),
    ];
    for (exit, code) in all {
        assert_eq!(exit.code(), code);
        assert_eq!(Exit::from_code(code), Some(exit));
    }
    // pkexec's dismissed and not authorised, a panic, rpm's own failure.
    for other in [1, 101, 126, 127] {
        assert_eq!(Exit::from_code(other), None, "{other}");
    }
}

/// The system task: the unit by its fixed name, waited for, its output on
/// the helper's pipes, unloaded even after a failure, with a `/tmp` of its
/// own; inside it the
/// shutdown inhibitor, inside that `helper run <hash>`; every program by its
/// absolute path.
#[test]
fn the_unit_wraps_the_inhibitor_which_wraps_run() {
    let h = "a".repeat(64);
    assert_eq!(
        unit_argv(&h),
        [
            "/usr/bin/systemd-run",
            "--wait",
            "--pipe",
            "--collect",
            "--quiet",
            "--property=PrivateTmp=yes",
            "--unit=lmgw-machine-setup",
            "--description=lmgw: setting up the graphics card",
            "/usr/bin/systemd-inhibit",
            "--what=shutdown:sleep",
            "--who=lmgw",
            "--why=Setting up the graphics card for local models",
            "--mode=block",
            "/usr/libexec/lmgw/lmgw-machine-helper",
            "run",
            h.as_str(),
        ]
    );
}

#[test]
fn a_plan_hash_is_64_lowercase_hex_digits() {
    assert!(is_plan_hash(&"0123456789abcdef".repeat(4)));
    assert!(!is_plan_hash(&"0123456789ABCDEF".repeat(4)), "uppercase");
    assert!(!is_plan_hash(&"a".repeat(63)));
    assert!(!is_plan_hash(&"a".repeat(65)));
    assert!(!is_plan_hash(&format!("{}g", "a".repeat(63))));
    assert!(!is_plan_hash(&format!("-{}", "a".repeat(63))));
    assert!(!is_plan_hash(""));
}

#[test]
fn a_running_unit_is_any_state_but_inactive_and_failed() {
    for s in [
        "active",
        "activating",
        "deactivating",
        "reloading",
        "refreshing",
        "active\n",
    ] {
        assert!(unit_running(s), "{s:?}");
    }
    for s in ["inactive", "failed", "", "maintenance"] {
        assert!(!unit_running(s), "{s:?}");
    }
}

/// A step kind on the wire is the name the hash and the API use.
#[test]
fn step_kinds_on_the_wire_are_the_plans_names() {
    for kind in [
        StepKind::AddRepo,
        StepKind::EnableRepo,
        StepKind::AddKey,
        StepKind::Install,
        StepKind::Update,
        StepKind::Swap,
    ] {
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, format!("\"{}\"", kind.as_str()));
        assert_eq!(serde_json::from_str::<StepKind>(&json).unwrap(), kind);
    }
}

/// Every event the helper prints parses back as itself, and an event line
/// says what it is in its `event` field.
#[test]
fn every_event_line_parses_back() {
    let m = Machine::fedora();
    let plans: Vec<PlanVariant> = variants(&m.inputs(), PROVIDERS)
        .into_iter()
        .map(|(v, _)| v)
        .collect();
    let events = [
        Event::Begun {
            verb: "apply".into(),
            version: "0.5.0+1".into(),
            schema: PLAN_SCHEMA,
            pkexec_uid: Some(1000),
        },
        Event::Begun {
            verb: "run".into(),
            version: "0.5.0+1".into(),
            schema: PLAN_SCHEMA,
            pkexec_uid: None,
        },
        Event::Plan {
            schema: PLAN_SCHEMA,
            version: "0.5.0".into(),
            plans: plans.clone(),
        },
        Event::PlanChanged {
            asked: "b".repeat(64),
            schema: PLAN_SCHEMA,
            version: "0.5.0".into(),
            plans,
        },
        Event::Refused { why: "no".into() },
        Event::AlreadyRunning {
            state: "active".into(),
        },
        Event::NothingToDo,
        Event::Started {
            sha256: "c".repeat(64),
            steps: 3,
        },
        Event::StepStarted {
            step: 1,
            of: 3,
            kind: StepKind::AddRepo,
        },
        Event::Line {
            step: 3,
            kind: StepKind::Install,
            stream: Stream::Stderr,
            line: "Updating and loading repositories: \"quoted\" \t tab".into(),
        },
        Event::StepEnded {
            step: 3,
            kind: StepKind::Install,
            exit: None,
            signal: Some(9),
            error: None,
        },
        Event::Note {
            line: "systemd-run: Failed".into(),
        },
        Event::Finished { exit: 3 },
        Event::TaskFailed {
            why: "exit 1".into(),
        },
    ];
    for e in events {
        let line = e.to_line();
        assert!(!line.contains('\n'), "one line: {line}");
        assert_eq!(Event::parse(&line), Some(e.clone()), "{line}");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(v["event"].is_string(), "{line}");
    }
    assert_eq!(
        Event::StepStarted {
            step: 2,
            of: 3,
            kind: StepKind::AddKey
        }
        .to_line(),
        r#"{"event":"step_started","step":2,"of":3,"kind":"s2_add_key"}"#
    );
    // `run`'s begun has no pkexec_uid at all on the wire.
    assert_eq!(
        Event::Begun {
            verb: "run".into(),
            version: "0.5.0".into(),
            schema: 1,
            pkexec_uid: None,
        }
        .to_line(),
        r#"{"event":"begun","verb":"run","version":"0.5.0","schema":1}"#
    );
    assert_eq!(Event::parse("not json"), None);
    assert_eq!(Event::parse(r#"{"event":"unknown"}"#), None);
}

/// Root's plans are one per value of the flag only the user's side knows,
/// read once; their hashes are the accepted ones (D44).
#[test]
fn roots_variants_are_the_accepted_hashes() {
    let m = Machine::fedora();
    m.nvidia_toolkit().nvidia_section().trust_nvidia();
    let inputs: Inputs = m.inputs();
    let v = variants(&inputs, PROVIDERS);
    assert_eq!(v.len(), 2);
    assert!(!v[0].0.update_toolkit && v[1].0.update_toolkit);
    let mut hashes: Vec<String> = v.iter().map(|(p, _)| p.sha256.clone()).collect();
    hashes.dedup();
    assert_eq!(hashes, accepted_hashes(&inputs, PROVIDERS));
    for (variant, plan) in &v {
        assert_eq!(variant.sha256, plan.hash());
        assert_eq!(variant.plan, plan.hashed());
        assert!(variant.blocked.is_empty());
    }
    // NVIDIA's toolkit in place: nothing without the update; with it,
    // lmgw's key file and S4′.
    assert!(v[0].0.plan.steps.is_empty());
    let kinds: Vec<&str> = v[1].0.plan.steps.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(kinds, ["s2_add_key", "s4_update"]);
}
