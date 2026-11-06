//! `run <sha256>` (§3.1): as root inside the system task, compare again (the
//! machine could change between `apply` and the unit starting), then run the
//! plan's steps in order and stop at the first failure.
//!
//! Every step's program gets pipes of the helper's own; each of its lines,
//! and each step's start and end, goes out as an event on stdout and into
//! the journal. A program never writes into lmgw's pipe itself, and a dead
//! lmgw never stops a step. Nothing is read again afterwards: the app's own
//! check after the run decides whether the card works (§3.5).

use std::io::Write;

use lmgw_machine::helper::{Event, Exit, Stream};
use lmgw_machine::plan::{Step, PLAN_SCHEMA};
use lmgw_machine::Facts;

use crate::emit::Emitter;
use crate::host::Host;
use crate::{matched, not_runnable, plan_changed, Caller, VERSION};

pub fn run(
    sha256: &str,
    caller: &Caller,
    facts: &dyn Facts,
    host: &dyn Host,
    out: &mut dyn Write,
) -> Exit {
    // The caller first, and nothing journaled for one refused (as `apply`).
    if let Err(why) = caller.may_run() {
        Emitter::new(out, None).emit(&Event::Refused { why });
        return Exit::Refused;
    }
    let em = &mut Emitter::new(out, Some(host));
    em.emit(&Event::Begun {
        verb: "run".into(),
        version: VERSION.into(),
        schema: PLAN_SCHEMA,
        pkexec_uid: None,
    });
    let plan = match matched(sha256, facts) {
        Ok((_, plan)) => plan,
        Err(plans) => {
            em.emit(&plan_changed(sha256, plans));
            return Exit::PlanChanged;
        }
    };
    if !plan.steps.is_empty() {
        if let Some(why) = not_runnable(&plan) {
            em.emit(&Event::Refused { why });
            return Exit::Refused;
        }
    }
    em.emit(&Event::Started {
        sha256: sha256.into(),
        steps: plan.steps.len(),
    });
    let of = plan.steps.len();
    for (i, step) in plan.steps.iter().enumerate() {
        if let Err(failed) = run_step(i + 1, of, step, host, em) {
            em.emit(&failed);
            em.emit(&Event::Finished {
                exit: Exit::StepFailed.code(),
            });
            return Exit::StepFailed;
        }
    }
    em.emit(&Event::Finished {
        exit: Exit::Done.code(),
    });
    Exit::Done
}

/// One step: its file, then its commands. `Err` is the `StepEnded` that says
/// how it failed.
fn run_step(
    n: usize,
    of: usize,
    step: &Step,
    host: &dyn Host,
    em: &mut Emitter<'_>,
) -> Result<(), Event> {
    let kind = step.kind();
    let ended = |exit: Option<i32>, signal: Option<i32>, error: Option<String>| Event::StepEnded {
        step: n,
        kind,
        exit,
        signal,
        error,
    };
    let said = |line: String| Event::Line {
        step: n,
        kind,
        stream: Stream::Helper,
        line,
    };
    em.emit(&Event::StepStarted { step: n, of, kind });
    if let Some(w) = step.file() {
        em.emit(&said(format!(
            "{} {} (mode {:04o}, {} bytes)",
            if w.replace {
                "replacing lmgw's older text in"
            } else {
                "creating"
            },
            w.path,
            w.mode,
            w.content.len()
        )));
        if let Err(e) = host.write_file(w) {
            return Err(ended(
                None,
                None,
                Some(format!("{} could not be written: {e}", w.path)),
            ));
        }
    }
    for argv in step.commands() {
        em.emit(&said(format!("running {}", argv.join(" "))));
        let result = host.run(&argv, &mut |stream, line| {
            em.emit(&Event::Line {
                step: n,
                kind,
                stream,
                line: line.to_string(),
            })
        });
        match result {
            Ok(e) if e.ok() => {}
            Ok(e) => return Err(ended(e.code, e.signal, None)),
            Err(e) => {
                return Err(ended(
                    None,
                    None,
                    Some(format!("{} could not be started: {e}", argv[0])),
                ))
            }
        }
    }
    em.emit(&ended(Some(0), None, None));
    Ok(())
}
