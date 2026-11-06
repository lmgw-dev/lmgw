//! `apply <sha256>` (§3.1): as root through pkexec, recompute root's plans,
//! and only when the hash is one of them start the steps as the system task
//! `lmgw-machine-setup.service` under a shutdown inhibitor, and wait for it.
//!
//! pkexec starts the helper inside lmgw's own cgroup, the app's scope:
//! quitting lmgw tears down the pipe, and logging out or shutting down tears
//! down the scope, so `dnf` would die mid-transaction. The system task runs
//! in `system.slice`, outside the session (D22). Its fixed name is also the
//! lock: systemd refuses a second unit of that name while one is loaded.

use std::io::Write;
use std::path::Path;

use lmgw_machine::helper::{
    unit_argv, unit_running, unit_state_args, Event, Exit, Stream, SYSTEMCTL, SYSTEMD_RUN,
    UNIT_SERVICE,
};
use lmgw_machine::plan::PLAN_SCHEMA;
use lmgw_machine::Facts;

use crate::emit::Emitter;
use crate::host::Host;
use crate::{matched, not_runnable, plan_changed, Caller, VERSION};

/// Whether the system task runs, as systemd says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitState {
    Running(String),
    NotRunning(String),
    /// systemctl could not say; why.
    Unknown(String),
}

/// `systemctl show --property=ActiveState --value lmgw-machine-setup.service`.
pub fn unit_state(facts: &dyn Facts) -> UnitState {
    match facts.run(Path::new(SYSTEMCTL), &unit_state_args()) {
        Ok(out) if out.ok() => {
            let state = out.stdout.trim().to_string();
            if unit_running(&state) {
                UnitState::Running(state)
            } else {
                UnitState::NotRunning(state)
            }
        }
        Ok(out) => UnitState::Unknown(format!(
            "{SYSTEMCTL} show ended with {}: {}",
            out.status_text(),
            out.stderr.trim()
        )),
        Err(e) => UnitState::Unknown(format!("{SYSTEMCTL} could not be run: {e}")),
    }
}

/// The line of `run`'s that says how it ended, the last one relayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Said {
    Finished(i32),
    NothingToDo,
    PlanChanged,
    Refused,
}

impl Said {
    fn of(e: &Event) -> Option<Self> {
        Some(match e {
            Event::Finished { exit } => Self::Finished(*exit),
            Event::NothingToDo => Self::NothingToDo,
            Event::PlanChanged { .. } => Self::PlanChanged,
            Event::Refused { .. } => Self::Refused,
            _ => return None,
        })
    }

    fn name(self) -> String {
        match self {
            Self::Finished(exit) => format!("finished (exit {exit})"),
            Self::NothingToDo => "nothing_to_do".into(),
            Self::PlanChanged => "plan_changed".into(),
            Self::Refused => "refused".into(),
        }
    }
}

/// `run`'s exit code, as systemd-run passed it on, when `run`'s own line
/// says the same: 0 with `finished` 0 or `nothing_to_do`, 2 with
/// `plan_changed`, 3 with `finished` 3, 4 with `refused`. An exit code alone
/// could be systemd-run's, the inhibitor's or anything else the unit ran.
fn runs_own(code: Option<i32>, said: Option<Said>) -> Option<Exit> {
    let exit = Exit::from_code(code?)?;
    let agrees = match exit {
        Exit::Done => matches!(said, Some(Said::Finished(0) | Said::NothingToDo)),
        Exit::PlanChanged => said == Some(Said::PlanChanged),
        Exit::StepFailed => said == Some(Said::Finished(3)),
        Exit::Refused => said == Some(Said::Refused),
        Exit::AlreadyRunning | Exit::TaskFailed => false,
    };
    agrees.then_some(exit)
}

pub fn apply(
    sha256: &str,
    caller: &Caller,
    facts: &dyn Facts,
    host: &dyn Host,
    out: &mut dyn Write,
) -> Exit {
    // The caller first, and nothing journaled for one refused: the journal
    // is root's record of what root did, and anyone can start this program.
    if let Err(why) = caller.may_apply() {
        Emitter::new(out, None).emit(&Event::Refused { why });
        return Exit::Refused;
    }
    let em = &mut Emitter::new(out, Some(host));
    em.emit(&Event::Begun {
        verb: "apply".into(),
        version: VERSION.into(),
        schema: PLAN_SCHEMA,
        pkexec_uid: caller.asker(),
    });
    let plan = match matched(sha256, facts) {
        Ok((_, plan)) => plan,
        Err(plans) => {
            em.emit(&plan_changed(sha256, plans));
            return Exit::PlanChanged;
        }
    };
    if plan.steps.is_empty() {
        em.emit(&Event::NothingToDo);
        return Exit::Done;
    }
    if let Some(why) = not_runnable(&plan) {
        em.emit(&Event::Refused { why });
        return Exit::Refused;
    }
    match unit_state(facts) {
        UnitState::Running(state) => {
            em.emit(&Event::AlreadyRunning { state });
            return Exit::AlreadyRunning;
        }
        UnitState::NotRunning(_) => {}
        UnitState::Unknown(why) => em.emit(&Event::Note {
            line: format!(
                "could not ask systemd whether {UNIT_SERVICE} runs ({why}); starting it anyway, \
                 and systemd refuses a second unit of that name"
            ),
        }),
    }

    // The unit's lines are `run`'s JSON events (journaled by `run` itself):
    // passed on as they come. Anything else on its stdout, and everything on
    // its stderr (systemd-run's and the inhibitor's own words), goes on as
    // a note, so lmgw's pipe carries events only.
    let mut started = false;
    let mut said = None;
    let ended = host.run(&unit_argv(sha256), &mut |stream, line| match stream {
        Stream::Stdout => match Event::parse(line) {
            Some(e) => {
                if matches!(e, Event::Started { .. }) {
                    started = true;
                }
                if let Some(s) = Said::of(&e) {
                    said = Some(s);
                }
                em.relay(line);
            }
            None => em.emit(&Event::Note {
                line: format!("{UNIT_SERVICE} said: {line}"),
            }),
        },
        Stream::Stderr | Stream::Helper => em.emit(&Event::Note {
            line: format!("{SYSTEMD_RUN}: {line}"),
        }),
    });
    let ended = match ended {
        Ok(ended) => ended,
        Err(e) => {
            em.emit(&Event::TaskFailed {
                why: format!("{SYSTEMD_RUN} could not be started: {e}"),
            });
            return Exit::TaskFailed;
        }
    };
    // systemd-run --wait passes the unit's exit status on, and the inhibitor
    // its program's: `run`'s own four, when `run`'s line says the same.
    if let Some(exit) = runs_own(ended.code, said) {
        return exit;
    }
    // A unit another start began between the question above and this one:
    // systemd refused the name, and the other one runs.
    if !started {
        if let UnitState::Running(state) = unit_state(facts) {
            em.emit(&Event::AlreadyRunning { state });
            return Exit::AlreadyRunning;
        }
    }
    let how = match (ended.code, ended.signal) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(sig)) => format!("signal {sig}"),
        (None, None) => "no exit code".to_string(),
    };
    let last = match said {
        Some(s) => format!("the helper's run said {}", s.name()),
        None => "the helper's run said nothing of how it ended".to_string(),
    };
    em.emit(&Event::TaskFailed {
        why: if started {
            format!(
                "{SYSTEMD_RUN} ended with {how} after the steps began, and {last}; the journal \
                 (journalctl -u {UNIT_SERVICE}) has every line it wrote"
            )
        } else {
            format!(
                "{SYSTEMD_RUN} ended with {how} before any step began, and {last}; its own words \
                 are in the notes above"
            )
        },
    });
    Exit::TaskFailed
}
