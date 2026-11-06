//! The helper's lines (§3.1): one JSON [`Event`] per line on stdout, the pipe
//! back to lmgw, and the same event in the journal
//! (`SYSLOG_IDENTIFIER=lmgw-machine-setup`) with a message a person can read.
//!
//! Every write error on stdout is ignored, as Rust's runtime ignores SIGPIPE:
//! lmgw quitting while a step runs closes the pipe, and the steps go on to
//! their end. In the real process stdout is [`crate::stdout::Queued`], so a
//! reader that stops reading does not stop them either (D100). The journal
//! is the record that outlives the pipe; a journal that does not take a
//! line is said once on stdout.

use std::io::Write;

use lmgw_machine::helper::{Event, Stream, UNIT_SERVICE};

use crate::host::{priority, Host};

pub struct Emitter<'a> {
    out: &'a mut dyn Write,
    /// Where the journal entries go; `None` for `plan`, a refused grammar
    /// and a refused caller, which change nothing (D82).
    journal: Option<&'a dyn Host>,
    /// The journal refused a line already (said once).
    journal_failed: bool,
}

impl<'a> Emitter<'a> {
    pub fn new(out: &'a mut dyn Write, journal: Option<&'a dyn Host>) -> Self {
        Self {
            out,
            journal,
            journal_failed: false,
        }
    }

    /// `e` on stdout, and in the journal.
    pub fn emit(&mut self, e: &Event) {
        let line = e.to_line();
        self.write(&line);
        if let Some(host) = self.journal {
            let (prio, message) = journal_text(e);
            if let Err(err) = host.journal(prio, &message, &line) {
                if !self.journal_failed {
                    self.journal_failed = true;
                    let note = Event::Note {
                        line: format!(
                            "the journal did not take a line ({err}); this line and the output \
                             stay complete, and later journal failures are not repeated here"
                        ),
                    };
                    self.write(&note.to_line());
                }
            }
        }
    }

    /// A line from the system task, passed on as it came (it is `run`'s,
    /// which journaled it itself).
    pub fn relay(&mut self, line: &str) {
        self.write(line);
    }

    /// The line and its newline in one write, so a reader never gets one
    /// without the other where a pipe takes no more (D100).
    fn write(&mut self, line: &str) {
        let mut whole = Vec::with_capacity(line.len() + 1);
        whole.extend_from_slice(line.as_bytes());
        whole.push(b'\n');
        let _ = self.out.write_all(&whole);
        let _ = self.out.flush();
    }
}

/// The journal's message and priority for `e`: technical, but whole.
pub fn journal_text(e: &Event) -> (u8, String) {
    use priority::{ERR, INFO, NOTICE, WARNING};
    match e {
        Event::Begun {
            verb,
            version,
            schema,
            pkexec_uid,
        } => (
            INFO,
            format!(
                "lmgw-machine-helper {verb} begun (lmgw {version}, plan schema {schema}){}",
                match pkexec_uid {
                    Some(uid) => format!(", asked for by user {uid} through pkexec"),
                    None => String::new(),
                }
            ),
        ),
        Event::Plan { plans, .. } => (
            INFO,
            format!(
                "root's plans for this computer: {}",
                plans
                    .iter()
                    .map(|p| p.sha256.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        Event::PlanChanged { asked, plans, .. } => (
            WARNING,
            format!(
                "the plan asked for ({asked}) is none of root's plans for this computer ({}); \
                 nothing was changed",
                plans
                    .iter()
                    .map(|p| format!(
                        "{} with{} the toolkit update: {} step(s)",
                        p.sha256,
                        if p.update_toolkit { "" } else { "out" },
                        p.plan.steps.len()
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        ),
        Event::Refused { why } => (WARNING, format!("refused: {why}")),
        Event::AlreadyRunning { state } => (
            NOTICE,
            format!("{UNIT_SERVICE} is already {state}; no second one was started"),
        ),
        Event::NothingToDo => (
            INFO,
            "the plan asked for has no steps; nothing was started".into(),
        ),
        Event::Started { sha256, steps } => {
            (INFO, format!("running plan {sha256}: {steps} step(s)"))
        }
        Event::StepStarted { step, of, kind } => (
            INFO,
            format!("step {step} of {of} ({}) started", kind.as_str()),
        ),
        Event::Line {
            step,
            kind,
            stream,
            line,
        } => (
            if *stream == Stream::Stderr {
                NOTICE
            } else {
                INFO
            },
            format!(
                "step {step} ({}) {}: {line}",
                kind.as_str(),
                match stream {
                    Stream::Stdout => "stdout",
                    Stream::Stderr => "stderr",
                    Stream::Helper => "helper",
                }
            ),
        ),
        Event::StepEnded {
            step,
            kind,
            exit,
            signal,
            error,
        } => {
            let how = match (exit, signal, error) {
                (_, _, Some(error)) => format!("failed: {error}"),
                (Some(0), _, _) => "done".to_string(),
                (Some(code), _, _) => format!("failed: exit {code}"),
                (None, Some(sig), _) => format!("failed: killed by signal {sig}"),
                (None, None, None) => "failed: ended without an exit code".to_string(),
            };
            let prio = if *exit == Some(0) && error.is_none() {
                INFO
            } else {
                ERR
            };
            (prio, format!("step {step} ({}) {how}", kind.as_str()))
        }
        Event::Note { line } => (NOTICE, line.clone()),
        Event::Finished { exit } => match exit {
            0 => (INFO, "finished: every step ran".into()),
            code => (ERR, format!("stopped at a failed step (exit {code})")),
        },
        Event::TaskFailed { why } => (ERR, format!("the system task failed: {why}")),
    }
}
