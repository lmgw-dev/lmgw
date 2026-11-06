//! The progress view (machine-check design §3.5, §4.3, §4.4): what the
//! setup is doing while `machine_apply` runs, read from its events, and how
//! it ended.
//!
//! [`Progress`] is the state, changed only by [`Progress::take`] (one event)
//! and [`Progress::ended`] (the command's answer), so it is tested without a
//! page. The step list starts as the shown plan's bullets: the shell runs the
//! plan with that plan's hash, so step `n` of the events is the shown plan's
//! `n`th step. A setup followed from before may run another plan; its steps
//! are then named by their kind.
//!
//! A `line` or `note` is a program's output, byte for byte: it is rendered as
//! a text node, never as markup.

use leptos::prelude::*;
use lmgw_api_types::machine::{
    MachineApplyEvent, MachineApplyOutcome, MachineApplyResult, MachineLineStream, PlanStep,
    PlanStepKind,
};

/// Where the setup is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Phase {
    /// "Set it up" was pressed here; the shell checks again and shows its
    /// own confirmation.
    Confirming,
    /// The computer asks for the password (`password` until `authorized`).
    Password,
    /// The password was accepted: the steps run.
    Running,
    /// A setup from before runs; lmgw follows it (`following`).
    Following,
    /// lmgw checks this computer again (`checking`).
    Checking,
    /// The command answered.
    Ended,
}

/// One step's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StepState {
    Waiting,
    Running,
    Done,
    /// It did not finish: the exit code, signal or error, for the tooltip.
    Failed(String),
}

/// One row of the step list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepRow {
    pub kind: Option<PlanStepKind>,
    pub label: String,
    pub state: StepState,
}

/// One line of output, as it came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutLine {
    /// `None`: a note (the system task's own words, a journal note).
    pub stream: Option<MachineLineStream>,
    pub text: String,
}

/// The whole progress view's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Progress {
    pub phase: Phase,
    pub steps: Vec<StepRow>,
    pub lines: Vec<OutLine>,
    /// The setup runs as the system task: closing lmgw does not stop it.
    pub detached: bool,
    /// A setup from before is followed (`following`): its steps are known
    /// only from the journal, which this account may not read.
    pub followed: bool,
    pub result: Option<MachineApplyResult>,
    /// The command's own error (lmgw still starting, a refused document).
    pub error: Option<String>,
}

/// A step's name when the shown plan does not have it (a setup followed
/// from before): what the step does, in the plan's words.
pub(crate) fn kind_label(k: PlanStepKind) -> &'static str {
    match k {
        PlanStepKind::S1AddRepo => "Add NVIDIA's software source",
        PlanStepKind::S1EnableRepo => "Switch on NVIDIA's software source",
        PlanStepKind::S2AddKey => "Add NVIDIA's signing key",
        PlanStepKind::S4Install => "Install NVIDIA's tool that lets models use the graphics card",
        PlanStepKind::S4Update => "Update NVIDIA's tool that lets models use the graphics card",
        PlanStepKind::S4Swap => {
            "Replace the piece Fedora stopped updating with NVIDIA's current one"
        }
    }
}

impl Progress {
    /// Before the first event: the shown plan's steps, waiting.
    pub(crate) fn new(plan: &[PlanStep]) -> Self {
        Self {
            phase: Phase::Confirming,
            steps: plan
                .iter()
                .map(|s| StepRow {
                    kind: Some(s.kind),
                    label: s.sentence.clone(),
                    state: StepState::Waiting,
                })
                .collect(),
            lines: Vec::new(),
            detached: false,
            followed: false,
            result: None,
            error: None,
        }
    }

    /// Row `step` (from 1) of kind `kind`, made when the list has none
    /// there or another kind there (a setup followed from before).
    fn row(&mut self, step: usize, kind: PlanStepKind) -> Option<&mut StepRow> {
        let i = step.checked_sub(1)?;
        if self.steps.get(i).is_some_and(|r| r.kind != Some(kind)) {
            self.steps[i] = StepRow {
                kind: Some(kind),
                label: kind_label(kind).to_string(),
                state: StepState::Waiting,
            };
        }
        while self.steps.len() <= i {
            self.steps.push(StepRow {
                kind: None,
                label: String::new(),
                state: StepState::Waiting,
            });
        }
        let r = &mut self.steps[i];
        if r.kind.is_none() {
            r.kind = Some(kind);
            r.label = kind_label(kind).to_string();
        }
        Some(r)
    }

    /// One event. Events after the answer change nothing: the answer has
    /// every line, and its check is what the page shows.
    pub(crate) fn take(&mut self, ev: MachineApplyEvent) {
        if self.phase == Phase::Ended {
            return;
        }
        match ev {
            MachineApplyEvent::Password => self.phase = Phase::Password,
            MachineApplyEvent::Authorized => {
                self.phase = Phase::Running;
                self.detached = true;
            }
            MachineApplyEvent::Following { .. } => {
                self.phase = Phase::Following;
                self.detached = true;
                self.followed = true;
            }
            MachineApplyEvent::Started { steps } => {
                if self.phase != Phase::Following {
                    self.phase = Phase::Running;
                }
                self.detached = true;
                if self.steps.len() != steps {
                    self.steps.truncate(steps);
                }
            }
            MachineApplyEvent::StepStarted { step, kind, .. } => {
                if let Some(r) = self.row(step, kind) {
                    r.state = StepState::Running;
                }
            }
            MachineApplyEvent::Line { stream, line, .. } => self.lines.push(OutLine {
                stream: Some(stream),
                text: line,
            }),
            MachineApplyEvent::StepEnded {
                step,
                kind,
                exit,
                signal,
                error,
            } => {
                if let Some(r) = self.row(step, kind) {
                    r.state = match (exit, signal, error) {
                        (Some(0), _, None) => StepState::Done,
                        (_, _, Some(e)) => StepState::Failed(e),
                        (_, Some(s), _) => StepState::Failed(format!("ended by signal {s}")),
                        (Some(c), _, _) => StepState::Failed(format!("exit code {c}")),
                        (None, None, None) => StepState::Failed("ended without a code".into()),
                    };
                }
            }
            MachineApplyEvent::Note { line } => self.lines.push(OutLine {
                stream: None,
                text: line,
            }),
            MachineApplyEvent::Finished { .. } => {}
            MachineApplyEvent::Checking => self.phase = Phase::Checking,
        }
    }

    /// The command's answer: a step still running when it came is done
    /// when every step ran, and did not finish otherwise (its own
    /// `step_ended` can trail the answer).
    pub(crate) fn ended(&mut self, r: Result<MachineApplyResult, String>) {
        self.phase = Phase::Ended;
        let applied = matches!(&r, Ok(r) if r.outcome == MachineApplyOutcome::Applied);
        for s in &mut self.steps {
            if s.state == StepState::Running {
                s.state = if applied {
                    StepState::Done
                } else {
                    StepState::Failed("did not finish".into())
                };
            }
        }
        match r {
            Ok(r) => self.result = Some(r),
            Err(e) => self.error = Some(e),
        }
    }

    /// Whether the command is still out.
    pub(crate) fn running(&self) -> bool {
        self.phase != Phase::Ended
    }

    /// Whether any step began: the step list is worth showing.
    pub(crate) fn any_began(&self) -> bool {
        self.steps.iter().any(|s| s.state != StepState::Waiting)
    }

    /// The step list as shown: the shown plan's steps from "Set it up" on;
    /// for a setup followed from before only once a step event came, since
    /// without the journal its steps would wait forever (review L9).
    pub(crate) fn shown_steps(&self) -> Vec<StepRow> {
        let shown = if self.followed {
            self.any_began()
        } else {
            self.running() || self.any_began()
        };
        if shown {
            self.steps.clone()
        } else {
            Vec::new()
        }
    }

    /// A followed setup whose steps lmgw cannot see (yet): no step event
    /// came while it runs.
    pub(crate) fn steps_unseen(&self) -> bool {
        self.followed && self.running() && !self.any_began()
    }

    /// "This keeps running if you close lmgw": while the steps run as the
    /// system task, not once lmgw checks again after them (review L8).
    pub(crate) fn keeps_running(&self) -> bool {
        self.running() && self.detached && self.phase != Phase::Checking
    }
}

/// What the person reads while the setup runs.
pub(crate) fn phase_sentence(p: &Progress) -> String {
    match p.phase {
        Phase::Confirming => "lmgw is asking you to confirm, in a window of its own.".into(),
        Phase::Password => "Your computer is asking for your password.".into(),
        Phase::Following => {
            "The setup is still running from before; lmgw follows it and says how it ends.".into()
        }
        Phase::Running => match p.steps.iter().position(|s| s.state == StepState::Running) {
            // The list below names the step; this says where in it the setup is.
            Some(i) => format!("Setting it up: step {} of {}.", i + 1, p.steps.len()),
            None => "Setting it up…".into(),
        },
        Phase::Checking => "Checking this computer again…".into(),
        Phase::Ended => String::new(),
    }
}

/// How an answer reads: its tone (`ok`, `warn`, `err`).
pub(crate) fn tone(o: MachineApplyOutcome) -> &'static str {
    use MachineApplyOutcome as O;
    match o {
        O::Applied | O::NothingToDo => "ok",
        O::Declined
        | O::Unavailable
        | O::PlanChanged
        | O::Updated
        | O::Dismissed
        | O::NoAgent
        | O::NotAuthorized
        | O::Ended => "warn",
        O::ReadDifferently | O::StepFailed | O::Refused | O::TaskFailed | O::Failed => "err",
    }
}

/// A line's class by where it came from.
pub(crate) fn line_class(stream: Option<MachineLineStream>) -> &'static str {
    match stream {
        Some(MachineLineStream::Stdout) => "mc-line",
        Some(MachineLineStream::Stderr) => "mc-line err",
        Some(MachineLineStream::Helper) => "mc-line helper",
        None => "mc-line note",
    }
}

/// A step's class and the note beside it.
fn step_look(s: &StepState, ended: bool) -> (&'static str, Option<&'static str>) {
    match s {
        StepState::Waiting if ended => ("waiting", Some("not run")),
        StepState::Waiting => ("waiting", None),
        StepState::Running => ("running", None),
        StepState::Done => ("done", None),
        StepState::Failed(_) => ("failed", Some("didn't finish")),
    }
}

/// The page's own scroller around `el`: the nearest ancestor that scrolls
/// vertically (the dashboard keeps one per page).
fn scroller_of(el: &web_sys::Element) -> Option<web_sys::Element> {
    let mut at = el.parent_element();
    while let Some(e) = at {
        let scrolls = window()
            .get_computed_style(&e)
            .ok()
            .flatten()
            .and_then(|st| st.get_property_value("overflow-y").ok())
            .is_some_and(|v| v == "auto" || v == "scroll");
        if scrolls && e.scroll_height() > e.client_height() {
            return Some(e);
        }
        at = e.parent_element();
    }
    None
}

/// Lines `from..` came into the output `out`: when the line before them
/// was in view (the person reads the end), the page's scroller follows to
/// the new end; when they scrolled away from it, it stays (D152, D161).
fn follow_end(out: &web_sys::Element, from: usize) {
    let Some(scroller) = scroller_of(out) else {
        return;
    };
    let view = scroller.get_bounding_client_rect();
    let lines = out.children();
    // The end before these lines: the last line then, or the box's top.
    let was = match from.checked_sub(1) {
        Some(i) => lines
            .item(i as u32)
            .map(|l| l.get_bounding_client_rect().bottom()),
        None => Some(out.get_bounding_client_rect().top()),
    };
    let Some(was) = was else { return };
    let in_view = was >= view.top() && was <= view.bottom() + 2.0;
    let Some(last) = lines.item(lines.length().saturating_sub(1)) else {
        return;
    };
    let below = last.get_bounding_client_rect().bottom() - view.bottom();
    if in_view && below > 0.0 {
        scroller.set_scroll_top(scroller.scroll_top() + below.ceil() as i32);
    }
}

/// The step list, the phase line and the live output, of the run in
/// `progress` (none: nothing).
///
/// Each part reads a memo of its own (review M2): an output line changes
/// the line count and nothing else, so the step list, the phase line and
/// its pulse stay as they are, and the output gets one row more, its text
/// read once. Only the phase line is a live region.
#[component]
pub(crate) fn ProgressView(progress: RwSignal<Option<Progress>>) -> impl IntoView {
    let read = move |f: fn(&Progress) -> bool| progress.with(|p| p.as_ref().is_some_and(f));
    let running = Memo::new(move |_| read(Progress::running));
    let password = Memo::new(move |_| read(|p| p.phase == Phase::Password));
    let keeps = Memo::new(move |_| read(Progress::keeps_running));
    let unseen = Memo::new(move |_| read(Progress::steps_unseen));
    let phase =
        Memo::new(move |_| progress.with(|p| p.as_ref().map(phase_sentence).unwrap_or_default()));
    let steps = Memo::new(move |_| {
        progress.with(|p| p.as_ref().map(Progress::shown_steps).unwrap_or_default())
    });
    let n_steps = Memo::new(move |_| steps.with(Vec::len));
    let count = Memo::new(move |_| progress.with(|p| p.as_ref().map_or(0, |p| p.lines.len())));
    let out: NodeRef<leptos::html::Div> = NodeRef::new();
    // After the new rows are laid out: follow the end when it was read.
    Effect::new(move |before: Option<usize>| {
        let n = count.get();
        if let Some(from) = before.filter(|b| n > *b) {
            request_animation_frame(move || {
                if let Some(el) = out.get_untracked() {
                    follow_end(&el, from);
                }
            });
        }
        n
    });
    let row = move |i: usize| {
        let s = Memo::new(move |_| steps.with(|s| s.get(i).cloned()));
        let look = move || s.with(|s| s.as_ref().map(|s| step_look(&s.state, !running.get())));
        let class = move || format!("mc-run-step {}", look().map_or("waiting", |l| l.0));
        let title = move || {
            s.with(|s| match s.as_ref().map(|s| &s.state) {
                Some(StepState::Failed(why)) => Some(why.clone()),
                _ => None,
            })
        };
        let mark = move || match s.with(|s| s.as_ref().map(|s| s.state.clone())) {
            Some(StepState::Done) => super::items::tick().into_any(),
            Some(StepState::Failed(_)) => "!".into_any(),
            _ => (i + 1).to_string().into_any(),
        };
        let label = move || s.with(|s| s.as_ref().map(|s| s.label.clone()).unwrap_or_default());
        view! {
            <li class=class title=title>
                <span class="mc-step-n">{mark}</span>
                <span class="mc-step-label">
                    {label}
                    {move || look().and_then(|l| l.1).map(|n| view! { <span class="mc-step-note">{n}</span> })}
                </span>
            </li>
        }
    };
    let line = move |i: usize| {
        // A line never changes once it came: read once, untracked.
        let (class, text) = progress.with_untracked(|p| {
            p.as_ref()
                .and_then(|p| p.lines.get(i))
                .map(|l| (line_class(l.stream), l.text.clone()))
                .unwrap_or((line_class(None), String::new()))
        });
        view! { <div class=class>{text}</div> }
    };
    view! {
        <div class="mc-run">
            // Sticks to the top of the page's scroller while the output
            // below grows past it (D161).
            <div class="mc-run-head" class:live=move || running.get()>
                <h4>
                    {move || {
                        if running.get() { "Setting up your graphics card" } else { "What the setup did" }
                    }}
                </h4>
                <Show when=move || running.get()>
                    <div class="mc-phase" aria-live="polite">
                        <span class="mc-pulse" aria-hidden="true"></span>
                        <span>{move || phase.get()}</span>
                    </div>
                </Show>
            </div>
            <Show when=move || running.get() && password.get()>
                <p class="mc-sub">
                    "If you're not an administrator, someone who is has to type theirs."
                </p>
            </Show>
            <Show when=move || unseen.get()>
                <p class="mc-sub">
                    "lmgw sees that it is still running. Its steps show here only when this \
                     account may read the system's log."
                </p>
            </Show>
            <Show when=move || { n_steps.get() > 0 }>
                <ol class="mc-run-steps">
                    <For each=move || 0..n_steps.get() key=|i| *i children=row/>
                </ol>
            </Show>
            <Show when=move || keeps.get()>
                <p class="mc-sub">
                    "This keeps running if you close lmgw. Only turning the computer off can \
                     still interrupt it."
                </p>
            </Show>
            <Show when=move || running.get() && count.get() != 0>
                <details class="mc-fold">
                    <summary>{move || format!("What the setup prints ({} lines)", count.get())}</summary>
                    <div class="mc-out" node_ref=out>
                        <For each=move || 0..count.get() key=|i| *i children=line/>
                    </div>
                </details>
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::machine::MachineApplyOutcome;

    fn step(kind: PlanStepKind, sentence: &str) -> PlanStep {
        PlanStep {
            kind,
            sentence: sentence.into(),
            commands: Vec::new(),
            writes_file: None,
            download_bytes: None,
            terminal: Vec::new(),
            package_scripts: None,
        }
    }

    fn answer(outcome: MachineApplyOutcome) -> MachineApplyResult {
        MachineApplyResult {
            outcome,
            sentence: "s".into(),
            step: None,
            output: Vec::new(),
            detail: Vec::new(),
            check: None,
        }
    }

    #[test]
    fn a_run_goes_from_the_password_through_its_steps() {
        let plan = [
            step(PlanStepKind::S1AddRepo, "Add the source"),
            step(PlanStepKind::S4Install, "Install the tool"),
        ];
        let mut p = Progress::new(&plan);
        assert_eq!(p.phase, Phase::Confirming);
        p.take(MachineApplyEvent::Password);
        assert_eq!(
            phase_sentence(&p),
            "Your computer is asking for your password."
        );
        assert!(!p.detached);
        p.take(MachineApplyEvent::Authorized);
        assert!(p.detached);
        p.take(MachineApplyEvent::Started { steps: 2 });
        p.take(MachineApplyEvent::StepStarted {
            step: 1,
            of: 2,
            kind: PlanStepKind::S1AddRepo,
        });
        assert_eq!(phase_sentence(&p), "Setting it up: step 1 of 2.");
        p.take(MachineApplyEvent::Line {
            step: 1,
            kind: PlanStepKind::S1AddRepo,
            stream: MachineLineStream::Helper,
            line: "<b>not markup</b>".into(),
        });
        p.take(MachineApplyEvent::StepEnded {
            step: 1,
            kind: PlanStepKind::S1AddRepo,
            exit: Some(0),
            signal: None,
            error: None,
        });
        p.take(MachineApplyEvent::StepStarted {
            step: 2,
            of: 2,
            kind: PlanStepKind::S4Install,
        });
        p.take(MachineApplyEvent::StepEnded {
            step: 2,
            kind: PlanStepKind::S4Install,
            exit: Some(1),
            signal: None,
            error: None,
        });
        p.take(MachineApplyEvent::Finished { exit: 3 });
        p.take(MachineApplyEvent::Checking);
        assert_eq!(p.phase, Phase::Checking);
        assert_eq!(p.steps[0].state, StepState::Done);
        assert_eq!(p.steps[1].state, StepState::Failed("exit code 1".into()));
        assert_eq!(p.lines[0].text, "<b>not markup</b>");
        p.ended(Ok(answer(MachineApplyOutcome::StepFailed)));
        assert!(!p.running());
        // An event that trails the answer changes nothing.
        p.take(MachineApplyEvent::Note {
            line: "late".into(),
        });
        assert_eq!(p.lines.len(), 1);
    }

    #[test]
    fn a_followed_setup_names_steps_the_shown_plan_does_not_have() {
        let mut p = Progress::new(&[step(PlanStepKind::S4Install, "Install the tool")]);
        p.take(MachineApplyEvent::Following {
            state: "active".into(),
        });
        assert!(p.detached);
        p.take(MachineApplyEvent::StepStarted {
            step: 2,
            of: 2,
            kind: PlanStepKind::S4Swap,
        });
        assert_eq!(p.steps.len(), 2);
        assert_eq!(p.steps[1].label, kind_label(PlanStepKind::S4Swap));
        assert_eq!(p.steps[1].state, StepState::Running);
        // Another kind at a shown step's place replaces its name.
        p.take(MachineApplyEvent::StepStarted {
            step: 1,
            of: 2,
            kind: PlanStepKind::S1AddRepo,
        });
        assert_eq!(p.steps[0].label, kind_label(PlanStepKind::S1AddRepo));
        assert_eq!(p.phase, Phase::Following);
    }

    #[test]
    fn a_followed_setup_shows_no_steps_it_cannot_see() {
        // Without the journal no step event comes: the shown plan's steps
        // would wait forever, so the list waits for one (review L9).
        let mut p = Progress::new(&[step(PlanStepKind::S4Install, "Install the tool")]);
        assert_eq!(
            p.shown_steps().len(),
            1,
            "a run of its own lists them at once"
        );
        p.take(MachineApplyEvent::Following {
            state: "active".into(),
        });
        assert!(p.shown_steps().is_empty());
        assert!(p.steps_unseen());
        p.take(MachineApplyEvent::Checking);
        p.ended(Ok(answer(MachineApplyOutcome::Ended)));
        assert!(p.shown_steps().is_empty(), "nor after it ended");
        assert!(!p.steps_unseen());
        // With the journal, the steps show once one is seen.
        let mut q = Progress::new(&[step(PlanStepKind::S4Install, "Install the tool")]);
        q.take(MachineApplyEvent::Following {
            state: "active".into(),
        });
        q.take(MachineApplyEvent::StepStarted {
            step: 1,
            of: 1,
            kind: PlanStepKind::S4Install,
        });
        assert_eq!(q.shown_steps().len(), 1);
        assert!(!q.steps_unseen());
    }

    #[test]
    fn the_keeps_running_line_is_for_the_steps_only() {
        let mut p = Progress::new(&[step(PlanStepKind::S4Install, "Install the tool")]);
        p.take(MachineApplyEvent::Password);
        assert!(!p.keeps_running(), "nothing runs as root yet");
        p.take(MachineApplyEvent::Authorized);
        assert!(p.keeps_running());
        // lmgw checks again after the steps: nothing is left to interrupt
        // (review L8).
        p.take(MachineApplyEvent::Checking);
        assert!(!p.keeps_running());
    }

    #[test]
    fn the_answer_settles_a_step_still_running() {
        let plan = [step(PlanStepKind::S4Install, "Install the tool")];
        let mut p = Progress::new(&plan);
        p.take(MachineApplyEvent::StepStarted {
            step: 1,
            of: 1,
            kind: PlanStepKind::S4Install,
        });
        let mut q = p.clone();
        p.ended(Ok(answer(MachineApplyOutcome::Applied)));
        assert_eq!(p.steps[0].state, StepState::Done);
        q.ended(Err("lmgw is still starting".into()));
        assert!(matches!(q.steps[0].state, StepState::Failed(_)));
        assert_eq!(q.error.as_deref(), Some("lmgw is still starting"));
    }

    #[test]
    fn every_outcome_has_a_tone() {
        assert_eq!(tone(MachineApplyOutcome::Applied), "ok");
        assert_eq!(tone(MachineApplyOutcome::Dismissed), "warn");
        assert_eq!(tone(MachineApplyOutcome::StepFailed), "err");
    }
}
