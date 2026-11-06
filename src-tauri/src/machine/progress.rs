//! The helper's lines as the command's events (§3.5): what each line is
//! for the person watching, and what the command keeps of them to say how
//! the setup ended.
//!
//! One reader for both sources: the helper's own stdout while `apply` runs,
//! and the unit's journal entries while a setup that began earlier is
//! followed ([`super::follow`]). A line's text is a program's output and
//! goes on as it came, as text: nothing is stripped, shortened or turned
//! into markup here, and the page shows it as text (WP8a).

use lmgw_api_types::machine::{MachineApplyEvent, MachineLineStream, PlanStepKind};
use lmgw_machine::helper::{Event, PlanVariant, Stream};
use lmgw_machine::plan::StepKind;

use super::Sink;

/// The plan's step kind as the API names it.
pub(crate) fn kind(k: StepKind) -> PlanStepKind {
    match k {
        StepKind::AddRepo => PlanStepKind::S1AddRepo,
        StepKind::EnableRepo => PlanStepKind::S1EnableRepo,
        StepKind::AddKey => PlanStepKind::S2AddKey,
        StepKind::Install => PlanStepKind::S4Install,
        StepKind::Update => PlanStepKind::S4Update,
        StepKind::Swap => PlanStepKind::S4Swap,
    }
}

fn stream(s: Stream) -> MachineLineStream {
    match s {
        Stream::Stdout => MachineLineStream::Stdout,
        Stream::Stderr => MachineLineStream::Stderr,
        Stream::Helper => MachineLineStream::Helper,
    }
}

/// The helper's `plan_changed` line (exit 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanChanged {
    pub schema: u32,
    pub version: String,
    pub plans: Vec<PlanVariant>,
}

/// A step that did not end with exit 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FailedStep {
    pub step: usize,
    pub kind: PlanStepKind,
    pub exit: Option<i32>,
    pub signal: Option<i32>,
    pub error: Option<String>,
}

/// What the lines said so far.
#[derive(Debug, Default)]
pub(crate) struct Progress {
    /// A `begun` line came: pkexec let the helper run as root, so the
    /// password phase is over.
    pub authorized: bool,
    /// Every step line and note, in order, as text.
    pub output: Vec<String>,
    /// Each step line with its step, for the plain reason of a failure.
    pub step_lines: Vec<(usize, String)>,
    /// What the helper said about itself and anything that was not an
    /// event: the technical fold.
    pub detail: Vec<String>,
    pub plan_changed: Option<PlanChanged>,
    pub refused: Option<String>,
    pub already_running: Option<String>,
    pub nothing_to_do: bool,
    pub task_failed: Option<String>,
    /// `run`'s last line: 0 every step ran, 3 one failed.
    pub finished: Option<i32>,
    /// The last step that began.
    pub last_step: Option<(usize, PlanStepKind)>,
    /// The first step that did not end with exit 0.
    pub failed: Option<FailedStep>,
}

impl Progress {
    /// Take one line of the helper's stdout.
    pub(crate) fn line(&mut self, line: &str, sink: &dyn Sink) {
        match Event::parse(line) {
            Some(e) => self.event(e, sink),
            // `apply` relays events only (D76); anything else is said as it
            // came, so nothing it printed is lost.
            None => self.note(format!("the setup printed: {line}"), sink),
        }
    }

    fn note(&mut self, line: String, sink: &dyn Sink) {
        self.output.push(line.clone());
        sink.send(MachineApplyEvent::Note { line });
    }

    /// Take one of the helper's events, from its stdout or the unit's
    /// journal.
    pub(crate) fn event(&mut self, e: Event, sink: &dyn Sink) {
        match e {
            Event::Begun {
                verb,
                version,
                schema,
                pkexec_uid,
            } => {
                let by = pkexec_uid.map_or(String::new(), |u| format!(", asked by user {u}"));
                self.detail.push(format!(
                    "the helper's {verb} began as root: lmgw {version}, plan schema {schema}{by}"
                ));
                // `apply`'s first line: the password was accepted. `run`'s
                // (the unit's) comes after it and says nothing new here.
                if verb == "apply" && !self.authorized {
                    self.authorized = true;
                    sink.send(MachineApplyEvent::Authorized);
                }
            }
            Event::Started { sha256, steps } => {
                self.detail.push(format!(
                    "the system task runs the plan {sha256}: {steps} step(s)"
                ));
                sink.send(MachineApplyEvent::Started { steps });
            }
            Event::StepStarted { step, of, kind: k } => {
                let kind = kind(k);
                self.last_step = Some((step, kind));
                sink.send(MachineApplyEvent::StepStarted { step, of, kind });
            }
            Event::Line {
                step,
                kind: k,
                stream: s,
                line,
            } => {
                self.output.push(line.clone());
                self.step_lines.push((step, line.clone()));
                sink.send(MachineApplyEvent::Line {
                    step,
                    kind: kind(k),
                    stream: stream(s),
                    line,
                });
            }
            Event::StepEnded {
                step,
                kind: k,
                exit,
                signal,
                error,
            } => {
                let kind = kind(k);
                if exit != Some(0) && self.failed.is_none() {
                    self.failed = Some(FailedStep {
                        step,
                        kind,
                        exit,
                        signal,
                        error: error.clone(),
                    });
                }
                sink.send(MachineApplyEvent::StepEnded {
                    step,
                    kind,
                    exit,
                    signal,
                    error,
                });
            }
            Event::Note { line } => self.note(line, sink),
            Event::Finished { exit } => {
                self.finished = Some(exit);
                sink.send(MachineApplyEvent::Finished { exit });
            }
            Event::PlanChanged {
                asked,
                schema,
                version,
                plans,
            } => {
                self.detail.push(format!(
                    "the helper computed another plan than {asked} (lmgw {version}, plan schema \
                     {schema}) and changed nothing"
                ));
                self.plan_changed = Some(PlanChanged {
                    schema,
                    version,
                    plans,
                });
            }
            Event::Refused { why } => {
                self.detail.push(format!("the helper refused: {why}"));
                self.refused = Some(why);
            }
            Event::AlreadyRunning { state } => {
                self.detail
                    .push(format!("the system task runs already ({state})"));
                self.already_running = Some(state);
            }
            Event::NothingToDo => {
                self.detail
                    .push("the plan had no steps: nothing was started".into());
                self.nothing_to_do = true;
            }
            Event::TaskFailed { why } => {
                self.detail.push(format!("the system task failed: {why}"));
                self.task_failed = Some(why);
            }
            // `plan`'s answer; `apply` never prints it.
            Event::Plan { .. } => self
                .detail
                .push("the helper printed its plans, which apply never does".into()),
        }
    }
}
