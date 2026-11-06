//! The argument grammar (§3.1, §6 "The helper"): one verb, and for `apply`
//! and `run` one hash. Anything else exits 4 with the usage, before anything
//! is read or run.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use lmgw_machine::helper::{Event, Exit};
use lmgw_machine_helper::{grammar, Verb, VERSION};

use super::support::{helper, helper_os, in_the_unit, through_pkexec, FakeHost, Machine};

fn refused(args: &[&str]) {
    let m = Machine::fedora();
    let host = FakeHost::new();
    for caller in [through_pkexec(), in_the_unit()] {
        let o = helper(args, &caller, &m, &host);
        assert_eq!(o.exit, Exit::Refused, "{args:?}");
        assert!(
            o.stderr.contains("usage: lmgw-machine-helper plan"),
            "{args:?}: {}",
            o.stderr
        );
        assert!(
            matches!(o.events.as_slice(), [Event::Refused { .. }]),
            "{args:?}: {:?}",
            o.events
        );
    }
    // Nothing read (no program the facts know was run), nothing started,
    // nothing written, nothing journaled.
    assert!(m.f.calls().is_empty(), "{args:?}: {:?}", m.f.calls());
    assert!(host.untouched(), "{args:?}");
    assert!(host.journal().is_empty(), "{args:?}");
}

#[test]
fn anything_but_the_grammar_is_refused_before_anything_runs() {
    let h = "0".repeat(64);
    refused(&[]);
    refused(&["--help"]);
    refused(&["install"]);
    refused(&["plan", "extra"]);
    refused(&["apply"]);
    refused(&["run"]);
    refused(&["apply", &h, "extra"]);
    refused(&["run", &h, &h]);
    refused(&["apply", &"0".repeat(63)]);
    refused(&["apply", &"0".repeat(65)]);
    refused(&["apply", &"A".repeat(64)]);
    refused(&["run", &format!("{}g", "0".repeat(63))]);
    refused(&["apply", &format!("-{}", "0".repeat(63))]);
    refused(&["apply", &format!("{h} ")]);
    refused(&["APPLY", &h]);
    refused(&["", &h]);
}

#[test]
fn an_argument_that_is_not_utf8_is_refused() {
    let m = Machine::fedora();
    let host = FakeHost::new();
    let args = [OsString::from("apply"), OsString::from_vec(vec![0xff; 64])];
    let o = helper_os(&args, &through_pkexec(), &m, &host, &mut Vec::new());
    assert_eq!(o.exit, Exit::Refused);
    assert!(o.stderr.contains("not valid UTF-8"), "{}", o.stderr);
    assert!(host.untouched());
}

#[test]
fn the_grammar_names_each_verb() {
    let h = "0123456789abcdef".repeat(4);
    let os = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
    assert_eq!(grammar::parse(&os(&["plan"])), Ok(Verb::Plan));
    assert_eq!(
        grammar::parse(&os(&["apply", &h])),
        Ok(Verb::Apply(h.clone()))
    );
    assert_eq!(grammar::parse(&os(&["run", &h])), Ok(Verb::Run(h.clone())));
    assert_eq!(
        grammar::parse(&os(&["frobnicate"])),
        Err("unknown verb 'frobnicate'".into())
    );
}

/// `plan` reads the machine and prints root's two plans; it starts nothing
/// and journals nothing, whoever runs it.
#[test]
fn plan_prints_roots_plans_and_changes_nothing() {
    let m = Machine::fedora();
    let host = FakeHost::new();
    let unprivileged = lmgw_machine_helper::Caller {
        ruid: 1000,
        euid: 1000,
        pkexec_uid: None,
        cgroup: Ok("0::/user.slice\n".into()),
    };
    let o = helper(&["plan"], &unprivileged, &m, &host);
    assert_eq!(o.exit, Exit::Done);
    let [Event::Plan {
        schema,
        version,
        plans,
    }] = o.events.as_slice()
    else {
        panic!("{:?}", o.events)
    };
    assert_eq!(*schema, lmgw_machine::plan::PLAN_SCHEMA);
    assert_eq!(version, VERSION);
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].sha256, m.hash(false));
    assert_eq!(plans[1].sha256, m.hash(true));
    let kinds: Vec<&str> = plans[0]
        .plan
        .steps
        .iter()
        .map(|s| s.kind.as_str())
        .collect();
    assert_eq!(kinds, ["s1_add_repo", "s2_add_key", "s4_install"]);
    assert!(host.untouched());
    assert!(host.journal().is_empty());
}
