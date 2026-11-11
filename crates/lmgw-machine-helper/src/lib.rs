//! lmgw's root helper (machine-check design §3.1, WP6): installs NVIDIA's
//! container toolkit, and the repository and key it comes from, after one
//! password, exactly as the plan the person confirmed says.
//!
//! ```text
//! lmgw-machine-helper plan             anyone: print root's plans for this computer
//! lmgw-machine-helper apply <sha256>   root, through /usr/bin/pkexec: compare, start the system task
//! lmgw-machine-helper run <sha256>     root, inside lmgw-machine-setup.service: compare, run the steps
//! ```
//!
//! It takes no input but a verb and a plan hash. Root reads the machine
//! itself through `SystemFacts::for_root` (D60: no inherited environment),
//! computes its plans with the same library code the check uses
//! ([`lmgw_machine::helper::variants`]), and acts only when the hash is one
//! of them (D2, D44). It prints one JSON line per
//! [`lmgw_machine::helper::Event`] and ends with an
//! [`lmgw_machine::helper::Exit`] code.
//!
//! - [`grammar`]: the closed argument grammar;
//! - [`caller`]: who started it and where it runs;
//! - [`emit`]: the lines on stdout and in the journal, write errors ignored;
//! - [`host`]: the seam every effect goes through (programs, files, the
//!   journal), so the tests run nothing;
//! - [`apply`], [`run`]: the two verbs that change something;
//! - [`real`]: the real host, and [`files`] the two ways it writes a file;
//! - [`stdout`]: the real process's stdout, a queue no reader can stall.

// The pkexec root helper of a Linux system: on any other system the library is empty and the
// binary says it runs on Linux only (not on Windows yet).
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::io::Write;

use lmgw_machine::helper::{variants, Event, Exit, PlanVariant};
use lmgw_machine::plan::{Inputs, Plan, PLAN_SCHEMA, PROVIDERS};
use lmgw_machine::Facts;

pub mod apply;
pub mod caller;
pub mod emit;
pub mod files;
pub mod grammar;
pub mod host;
pub mod real;
pub mod run;
pub mod stdout;

pub use caller::Caller;
pub use emit::Emitter;
pub use grammar::{Verb, USAGE};
pub use host::{Ended, Host};

/// The lmgw release this helper was built with, as the app's own: a line
/// that says another one tells the app it was updated while it ran (§3.5).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The helper's whole run, every effect through `facts` (reads) and `host`
/// (programs, files, the journal): `args` without the program's name.
pub fn main(
    args: &[OsString],
    caller: &Caller,
    facts: &dyn Facts,
    host: &dyn Host,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Exit {
    let verb = match grammar::parse(args) {
        Ok(verb) => verb,
        Err(why) => {
            let _ = writeln!(err, "lmgw-machine-helper: {why}\n{USAGE}");
            let _ = err.flush();
            Emitter::new(out, None).emit(&Event::Refused { why });
            return Exit::Refused;
        }
    };
    match verb {
        Verb::Plan => {
            let plans = root_plans(facts).into_iter().map(|(v, _)| v).collect();
            Emitter::new(out, None).emit(&Event::Plan {
                schema: PLAN_SCHEMA,
                version: VERSION.into(),
                plans,
            });
            Exit::Done
        }
        Verb::Apply(sha256) => apply::apply(&sha256, caller, facts, host, out),
        Verb::Run(sha256) => run::run(&sha256, caller, facts, host, out),
    }
}

/// Root's plans for this machine, read once: one per value of the flag only
/// the user's side knows (D44).
pub fn root_plans(facts: &dyn Facts) -> Vec<(PlanVariant, Plan)> {
    variants(&Inputs::read(facts, PROVIDERS), PROVIDERS)
}

/// The plan whose hash is `sha256`, or every plan root has when none is.
pub fn matched(sha256: &str, facts: &dyn Facts) -> Result<(PlanVariant, Plan), Vec<PlanVariant>> {
    let plans = root_plans(facts);
    match plans.iter().position(|(v, _)| v.sha256 == sha256) {
        Some(i) => Ok(plans.into_iter().nth(i).expect("the index just found")),
        None => Err(plans.into_iter().map(|(v, _)| v).collect()),
    }
}

/// The line before exit 2: the hash asked for, and root's plans.
pub fn plan_changed(asked: &str, plans: Vec<PlanVariant>) -> Event {
    Event::PlanChanged {
        asked: asked.into(),
        schema: PLAN_SCHEMA,
        version: VERSION.into(),
        plans,
    }
}

/// Why a matched plan is not the helper's to run, if it is not: the helper
/// runs dnf5 plans only. An image-based system's or another system's steps
/// are shown as commands to type (§3.7) and never offered as the button.
pub fn not_runnable(p: &Plan) -> Option<String> {
    use lmgw_machine::plan::System;
    match p.system {
        System::Dnf5 => None,
        other => Some(format!(
            "this computer's plan is for {}, whose steps lmgw shows as commands to type; the \
             helper runs plans for dnf5 only",
            match other {
                System::Ostree => "an image-based system (rpm-ostree)",
                System::OtherDnf => "a dnf older than dnf5",
                System::NoDnf => "a system without dnf",
                System::Dnf5 => unreachable!(),
            }
        )),
    }
}
