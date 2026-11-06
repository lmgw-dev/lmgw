//! The shell command (machine-check design §6, "The shell command"): every
//! gate, the confirmation, each row of §3.5's table, exit 2's three
//! branches, following a running unit and what runs afterwards, against a
//! stated world. Nothing here runs pkexec, the helper, systemctl or
//! journalctl, or shows a dialog: the world, the dialog and the helper are
//! fakes, and the real spawn refuses in a debug build anyway. The
//! confirmation's tests are in [`confirm`], a failed step's plain reasons in
//! [`reasons`].

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use lmgw_api_types::machine::{
    MachineApplyEvent, MachineApplyOutcome, MachineCheck, MachineLineStream, MachinePlan,
    MachineVerdict, PlanFile, PlanHow, PlanStep, PlanStepKind,
};
use lmgw_machine::helper::{Event, PlanVariant, Stream};
use lmgw_machine::plan::hash::{HashedFile, HashedPlan, HashedStep};
use lmgw_machine::plan::{StepKind, PLAN_SCHEMA};

use super::confirm::{Answer, Arming, Asked, Confirmation, Window, ARM_DELAY};
use super::follow::{self, Entry};
use super::gates::Host;
use super::spawn::{self, Ended, Helper};
use super::{apply, guarded, Erased, Sink, World, APP_VERSION};

mod confirm;
mod reasons;

const HASH: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const OTHER: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const ROOT: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const DNF: &[&str] = &[
    "/usr/bin/dnf5",
    "install",
    "-y",
    "--repo=lmgw-nvidia-container-toolkit",
    "nvidia-container-toolkit-base",
];
const REPO_TEXT: &str = "# Written by lmgw.\n[lmgw-nvidia-container-toolkit]\nenabled=1\n";

// --- The stated world -----------------------------------------------------

/// The plan the page showed: S1 and S4, offered as `how`.
fn plan(hash: &str, how: PlanHow) -> MachinePlan {
    MachinePlan {
        hash: hash.into(),
        how,
        steps: vec![
            PlanStep {
                kind: PlanStepKind::S1AddRepo,
                sentence: "Add NVIDIA's software source (nvidia.github.io)".into(),
                commands: vec![],
                writes_file: Some(PlanFile {
                    path: "/etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo".into(),
                    content: REPO_TEXT.into(),
                    mode: 0o644,
                    replaces: false,
                    key_fingerprint: None,
                }),
                download_bytes: None,
                terminal: vec![],
                package_scripts: None,
            },
            PlanStep {
                kind: PlanStepKind::S4Install,
                sentence: "Install NVIDIA's tool that lets models use the graphics card".into(),
                commands: vec![DNF.iter().map(|s| s.to_string()).collect()],
                writes_file: None,
                download_bytes: Some(8_500_000),
                terminal: vec![],
                package_scripts: Some(SCRIPTS.into()),
            },
        ],
    }
}

/// What the stated install step's package changes by itself.
const SCRIPTS: &str = lmgw_machine::plan::scripts::INSTALL_SCRIPTS;

/// The key step between the two, for the confirmation's fold.
fn plan_with_key() -> MachinePlan {
    let mut p = plan(HASH, PlanHow::Helper);
    p.steps.insert(
        1,
        PlanStep {
            kind: PlanStepKind::S2AddKey,
            sentence: "Add NVIDIA's signing key, so your computer checks that the piece below \
                       really comes from NVIDIA"
                .into(),
            commands: vec![vec![
                "/usr/bin/rpmkeys".into(),
                "--import".into(),
                "/etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit".into(),
            ]],
            writes_file: Some(PlanFile {
                path: "/etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit".into(),
                content: "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nmQINBF…\n".into(),
                mode: 0o644,
                replaces: false,
                key_fingerprint: Some("C95B 321B 61E8 8C18 09C4 F759 DDCA E044 F796 ECB0".into()),
            }),
            download_bytes: None,
            terminal: vec![],
            package_scripts: None,
        },
    );
    p
}

fn check(verdict: MachineVerdict, headline: &str, plan: Option<MachinePlan>) -> MachineCheck {
    MachineCheck {
        checked_at: "2026-01-01 12:00:00".into(),
        verdict,
        headline: headline.into(),
        items: vec![],
        plan,
        failed_start: None,
    }
}

/// The check before the fix: the button for [`HASH`].
fn fixable() -> MachineCheck {
    check(
        MachineVerdict::NeedsFix,
        "One piece is missing so models can use the graphics card. lmgw can set it up.",
        Some(plan(HASH, PlanHow::Helper)),
    )
}

fn ready() -> MachineCheck {
    check(
        MachineVerdict::Ready,
        "This computer is ready for local models.",
        None,
    )
}

/// The installed app, as the gates want it.
fn installed() -> Host {
    Host {
        linux: true,
        release: true,
        dev: false,
        exe: Ok(PathBuf::from("/usr/bin/lmgw")),
        installed_data_dir: true,
    }
}

/// What the stated helper prints and how it ends.
#[derive(Clone)]
struct Script {
    lines: Vec<String>,
    ended: Ended,
}

struct FakeHelper {
    lines: VecDeque<String>,
    ended: Ended,
}

impl Helper for FakeHelper {
    fn line(&mut self) -> Erased<'_, Option<String>> {
        Box::pin(async move { self.lines.pop_front() })
    }

    fn ended(self: Box<Self>) -> Erased<'static, Ended> {
        Box::pin(async move { self.ended })
    }
}

struct Fake {
    host: Host,
    /// What every look at the host after the first sees (an update that
    /// replaced the program while the dialog was open); `None`: `host`.
    host_later: Option<Host>,
    hosts_read: AtomicUsize,
    helper: Result<(), String>,
    /// systemd's answers in order; once spent, `inactive`.
    states: Mutex<VecDeque<Result<String, String>>>,
    invocation: Option<String>,
    /// The journal's answers in order; once spent, nothing new.
    journal: Mutex<VecDeque<Result<Vec<Entry>, String>>>,
    journal_asks: Mutex<Vec<(String, Option<String>)>>,
    /// The checks in order; the last one repeats.
    checks: Mutex<VecDeque<MachineCheck>>,
    checks_run: AtomicUsize,
    after: MachineCheck,
    after_runs: AtomicUsize,
    read_differently: Mutex<Vec<String>>,
    /// The app's window at each ask, in order; the last one repeats.
    windows: Mutex<VecDeque<Window>>,
    /// The confirmation's memory, as `machine::ASKED` is the real one's.
    asked: Asked,
    /// What the person presses in a dialog that shows.
    confirm: bool,
    /// Every dialog that showed.
    confirmations: Mutex<Vec<Confirmation>>,
    script: Option<Script>,
    spawned: Mutex<Vec<String>>,
}

impl Fake {
    fn new() -> Self {
        Self {
            host: installed(),
            host_later: None,
            hosts_read: AtomicUsize::new(0),
            helper: Ok(()),
            states: Mutex::new(VecDeque::new()),
            invocation: Some("0123abcd".into()),
            journal: Mutex::new(VecDeque::new()),
            journal_asks: Mutex::new(Vec::new()),
            checks: Mutex::new(VecDeque::from([fixable()])),
            checks_run: AtomicUsize::new(0),
            after: ready(),
            after_runs: AtomicUsize::new(0),
            read_differently: Mutex::new(Vec::new()),
            windows: Mutex::new(VecDeque::from([in_front(1)])),
            asked: Asked::new(),
            confirm: true,
            confirmations: Mutex::new(Vec::new()),
            script: None,
            spawned: Mutex::new(Vec::new()),
        }
    }

    fn running(self, events: &[Event], ended: Ended) -> Self {
        Self {
            script: Some(Script {
                lines: events.iter().map(Event::to_line).collect(),
                ended,
            }),
            ..self
        }
    }

    fn checks(self, checks: Vec<MachineCheck>) -> Self {
        *self.checks.lock().unwrap() = checks.into();
        self
    }

    fn states(self, states: &[&str]) -> Self {
        *self.states.lock().unwrap() = states.iter().map(|s| Ok(s.to_string())).collect();
        self
    }

    fn windows(self, windows: &[Window]) -> Self {
        *self.windows.lock().unwrap() = windows.iter().copied().collect();
        self
    }

    /// The program replaced after the first look (an RPM update).
    fn updated_later(self) -> Self {
        Self {
            host_later: Some(Host {
                exe: Ok(PathBuf::from("/usr/bin/lmgw (deleted)")),
                ..installed()
            }),
            ..self
        }
    }

    fn spawned(&self) -> usize {
        self.spawned.lock().unwrap().len()
    }

    fn asked(&self) -> usize {
        self.confirmations.lock().unwrap().len()
    }
}

/// The app's window shown and active, running document `n`.
fn in_front(n: u64) -> Window {
    Window {
        visible: true,
        active: true,
        document: Some(n),
    }
}

impl World for Fake {
    fn host(&self) -> Host {
        let first = self.hosts_read.fetch_add(1, Ordering::SeqCst) == 0;
        match &self.host_later {
            Some(later) if !first => later.clone(),
            _ => self.host.clone(),
        }
    }

    fn helper_installed(&self) -> Result<(), String> {
        self.helper.clone()
    }

    fn unit_state(&self) -> Erased<'_, Result<String, String>> {
        Box::pin(async move {
            self.states
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok("inactive".into()))
        })
    }

    fn invocation(&self) -> Erased<'_, Option<String>> {
        Box::pin(async move { self.invocation.clone() })
    }

    fn journal<'a>(
        &'a self,
        invocation: &'a str,
        cursor: Option<&'a str>,
    ) -> Erased<'a, Result<Vec<Entry>, String>> {
        Box::pin(async move {
            self.journal_asks
                .lock()
                .unwrap()
                .push((invocation.to_string(), cursor.map(str::to_string)));
            self.journal
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(Vec::new()))
        })
    }

    fn pause(&self) -> Erased<'_, ()> {
        Box::pin(async {})
    }

    fn check(&self) -> Erased<'_, MachineCheck> {
        Box::pin(async move {
            self.checks_run.fetch_add(1, Ordering::SeqCst);
            let mut checks = self.checks.lock().unwrap();
            if checks.len() > 1 {
                checks.pop_front().unwrap()
            } else {
                checks.front().cloned().expect("a stated check")
            }
        })
    }

    fn after_fix(&self) -> Erased<'_, (MachineCheck, Vec<String>)> {
        Box::pin(async move {
            self.after_runs.fetch_add(1, Ordering::SeqCst);
            (
                self.after.clone(),
                vec!["lmgw's own CDI spec after the setup: Written".into()],
            )
        })
    }

    fn read_differently(&self, hash: &str) {
        self.read_differently.lock().unwrap().push(hash.into());
    }

    /// As `confirm::ask` does on the GTK main thread: the window, then the
    /// dialog only where `Asked` lets it show, then the answer through
    /// `Asked` again.
    fn confirm(&self, c: Confirmation) -> Erased<'_, Answer> {
        Box::pin(async move {
            let window = {
                let mut windows = self.windows.lock().unwrap();
                if windows.len() > 1 {
                    windows.pop_front().unwrap()
                } else {
                    *windows.front().expect("a stated window")
                }
            };
            match self.asked.may_show(window) {
                Err(answer) => answer,
                Ok(document) => {
                    self.confirmations.lock().unwrap().push(c);
                    self.asked.answered(document, self.confirm)
                }
            }
        })
    }

    fn spawn(&self, hash: &str) -> Result<Box<dyn Helper>, String> {
        self.spawned.lock().unwrap().push(hash.into());
        let s = self.script.clone().expect("no helper was stated");
        Ok(Box::new(FakeHelper {
            lines: s.lines.into(),
            ended: s.ended,
        }))
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<MachineApplyEvent>>);

impl Sink for Events {
    fn send(&self, event: MachineApplyEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Events {
    fn all(&self) -> Vec<MachineApplyEvent> {
        self.0.lock().unwrap().clone()
    }
}

fn exit(code: i32) -> Ended {
    Ended {
        code: Some(code),
        ..Ended::default()
    }
}

fn pkexec_says(code: i32, stderr: &str) -> Ended {
    Ended {
        code: Some(code),
        stderr: stderr.into(),
        ..Ended::default()
    }
}

fn begun(verb: &str) -> Event {
    Event::Begun {
        verb: verb.into(),
        version: APP_VERSION.into(),
        schema: PLAN_SCHEMA,
        pkexec_uid: (verb == "apply").then_some(1000),
    }
}

fn line(step: usize, kind: StepKind, stream: Stream, text: &str) -> Event {
    Event::Line {
        step,
        kind,
        stream,
        line: text.into(),
    }
}

fn step_ended(step: usize, kind: StepKind, exit: i32) -> Event {
    Event::StepEnded {
        step,
        kind,
        exit: Some(exit),
        signal: None,
        error: None,
    }
}

/// `run`'s lines for the two-step plan, the second step ending `last`.
fn run_lines(last: i32, dnf_says: &[&str]) -> Vec<Event> {
    let mut e = vec![
        begun("run"),
        Event::Started {
            sha256: HASH.into(),
            steps: 2,
        },
        Event::StepStarted {
            step: 1,
            of: 2,
            kind: StepKind::AddRepo,
        },
        line(
            1,
            StepKind::AddRepo,
            Stream::Helper,
            "creating /etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo",
        ),
        step_ended(1, StepKind::AddRepo, 0),
        Event::StepStarted {
            step: 2,
            of: 2,
            kind: StepKind::Install,
        },
    ];
    e.extend(
        dnf_says
            .iter()
            .map(|t| line(2, StepKind::Install, Stream::Stderr, t)),
    );
    e.push(step_ended(2, StepKind::Install, last));
    e.push(Event::Finished {
        exit: if last == 0 { 0 } else { 3 },
    });
    e
}

/// `apply`'s whole output around `run`'s.
fn applied_lines(run: Vec<Event>) -> Vec<Event> {
    std::iter::once(begun("apply")).chain(run).collect()
}

fn root_variant(sha256: &str) -> PlanVariant {
    PlanVariant {
        update_toolkit: false,
        sha256: sha256.into(),
        plan: HashedPlan {
            schema: PLAN_SCHEMA,
            steps: vec![
                HashedStep {
                    kind: "s1_add_repo".into(),
                    commands: vec![],
                    file: Some(HashedFile {
                        path: "/etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo".into(),
                        mode: 0o644,
                        replace: false,
                        content: REPO_TEXT.into(),
                    }),
                },
                HashedStep {
                    kind: "s4_install".into(),
                    commands: vec![DNF.iter().map(|s| s.to_string()).collect()],
                    file: None,
                },
            ],
        },
        blocked: vec![],
    }
}

fn plan_changed(version: &str, schema: u32, root: &[&str]) -> Event {
    Event::PlanChanged {
        asked: HASH.into(),
        schema,
        version: version.into(),
        plans: root.iter().map(|h| root_variant(h)).collect(),
    }
}

fn kinds(events: &[MachineApplyEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e {
            MachineApplyEvent::Password => "password",
            MachineApplyEvent::Authorized => "authorized",
            MachineApplyEvent::Following { .. } => "following",
            MachineApplyEvent::Started { .. } => "started",
            MachineApplyEvent::StepStarted { .. } => "step_started",
            MachineApplyEvent::Line { .. } => "line",
            MachineApplyEvent::StepEnded { .. } => "step_ended",
            MachineApplyEvent::Note { .. } => "note",
            MachineApplyEvent::Finished { .. } => "finished",
            MachineApplyEvent::Checking => "checking",
        })
        .collect()
}

async fn run(world: &Fake) -> (super::MachineApplyResult, Vec<MachineApplyEvent>) {
    let sink = Events::default();
    let r = apply(world, HASH, &sink).await;
    (r, sink.all())
}

// --- The gates (§3.4) ---------------------------------------------------------

/// Each gate refuses with a plain sentence and its reason in the detail,
/// before anything is asked, checked or started.
#[tokio::test]
async fn each_gate_refuses_with_its_reason_and_starts_nothing() {
    let cases: Vec<(Host, Result<(), String>, &str, &str)> = vec![
        (
            Host {
                linux: false,
                ..installed()
            },
            Ok(()),
            "only on Linux",
            "Linux only",
        ),
        (
            Host {
                release: false,
                ..installed()
            },
            Ok(()),
            "development build",
            "debug build",
        ),
        (
            Host {
                dev: true,
                ..installed()
            },
            Ok(()),
            "development copy",
            "dev instance",
        ),
        (
            Host {
                exe: Ok("/home/someone/lmgw/target/release/lmgw".into()),
                ..installed()
            },
            Ok(()),
            "isn't the installed one",
            "/home/someone/lmgw/target/release/lmgw",
        ),
        (
            Host {
                exe: Ok("/usr/bin/lmgw (deleted)".into()),
                ..installed()
            },
            Ok(()),
            "was updated while it was running",
            "replaced",
        ),
        (
            Host {
                exe: Err("no /proc".into()),
                ..installed()
            },
            Ok(()),
            "couldn't tell which program",
            "no /proc",
        ),
        (
            Host {
                installed_data_dir: false,
                ..installed()
            },
            Ok(()),
            "another folder",
            "LMGW_DATA_DIR",
        ),
        // `helper_installed`'s own reasons: a helper writable by others,
        // owned by a user, setuid, a symlink, the policy missing, no
        // /usr/bin/pkexec (another one on PATH is never looked at).
        (
            installed(),
            Err("/usr/libexec/lmgw/lmgw-machine-helper has mode 0777, more than 0755".into()),
            "no working setup helper",
            "mode 0777",
        ),
        (
            installed(),
            Err("/usr/libexec/lmgw/lmgw-machine-helper belongs to user 1000, not to root".into()),
            "no working setup helper",
            "user 1000",
        ),
        (
            installed(),
            Err("/usr/libexec/lmgw/lmgw-machine-helper is a symbolic link".into()),
            "no working setup helper",
            "symbolic link",
        ),
        (
            installed(),
            Err("the setup's polkit action is not installed (… is not there)".into()),
            "no working setup helper",
            "polkit action",
        ),
        (
            installed(),
            Err("/usr/bin/pkexec is not there".into()),
            "no working setup helper",
            "/usr/bin/pkexec",
        ),
    ];
    for (host, helper, says, why) in cases {
        let w = Fake {
            host,
            helper,
            ..Fake::new()
        };
        let (r, events) = run(&w).await;
        assert_eq!(r.outcome, MachineApplyOutcome::Unavailable, "{r:#?}");
        assert!(r.sentence.contains(says), "{says}: {}", r.sentence);
        assert!(
            r.detail.iter().any(|d| d.contains(why)),
            "{why}: {:?}",
            r.detail
        );
        assert!(r.check.is_none());
        assert_eq!(w.checks_run.load(Ordering::SeqCst), 0);
        assert_eq!((w.asked(), w.spawned()), (0, 0));
        assert!(events.is_empty());
    }
}

/// Not dnf5 (and every other system the check gives instructions): the
/// check's own reason, no dialog, no password.
#[tokio::test]
async fn a_plan_offered_as_instructions_is_refused_with_its_reason() {
    let why = "lmgw's commands are written for dnf5, and this system has an older dnf, so lmgw \
               shows none here.";
    let w = Fake::new().checks(vec![check(
        MachineVerdict::NeedsSteps,
        "One piece is missing so models can use the graphics card. Here's how to add it.",
        Some(plan(
            HASH,
            PlanHow::Instructions {
                why: why.to_string(),
            },
        )),
    )]);
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Unavailable);
    assert_eq!(r.sentence, why);
    assert!(r.check.is_some());
    assert_eq!((w.asked(), w.spawned()), (0, 0));
}

/// The page's plan is not the one a fresh check computes: the new check,
/// and nothing asked.
#[tokio::test]
async fn a_hash_the_fresh_check_does_not_compute_asks_nothing() {
    let w = Fake::new().checks(vec![check(
        MachineVerdict::NeedsFix,
        "…",
        Some(plan(OTHER, PlanHow::Helper)),
    )]);
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::PlanChanged);
    assert_eq!(r.check.unwrap().plan.unwrap().hash, OTHER);
    assert_eq!((w.asked(), w.spawned()), (0, 0));
}

/// Nothing left to do: no dialog.
#[tokio::test]
async fn a_check_with_no_plan_has_nothing_to_set_up() {
    let w = Fake::new().checks(vec![ready()]);
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::NothingToDo);
    assert!(
        r.sentence.contains("This computer is ready"),
        "{}",
        r.sentence
    );
    assert_eq!((w.asked(), w.spawned()), (0, 0));
}

/// A second call while one runs gets an answer at once, without a dialog.
#[tokio::test]
async fn a_second_call_while_one_runs_asks_nothing() {
    let flag = AtomicBool::new(true);
    let w = Fake::new();
    let sink = Events::default();
    let r = guarded(&flag, &w, HASH, &sink).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Unavailable);
    assert!(r.sentence.contains("already setting this up"));
    assert_eq!((w.asked(), w.spawned()), (0, 0));
    // Free again, the call goes through, and frees the flag after it.
    flag.store(false, Ordering::SeqCst);
    let w = Fake {
        confirm: false,
        ..Fake::new()
    };
    let r = guarded(&flag, &w, HASH, &sink).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Declined);
    assert!(!flag.load(Ordering::SeqCst));
}

// --- Running it (§3.5) ------------------------------------------------------

/// Exactly pkexec by its path, its agent off, the helper and its two
/// arguments.
#[test]
fn the_helper_is_started_by_absolute_paths_with_its_two_arguments() {
    assert_eq!(
        spawn::pkexec_args(HASH),
        [
            "--disable-internal-agent",
            "/usr/libexec/lmgw/lmgw-machine-helper",
            "apply",
            HASH
        ]
    );
    assert_eq!(lmgw_machine::plan::embedded::PKEXEC, "/usr/bin/pkexec");
}

/// A debug build's real spawn refuses below the gates (D4). Only built in a
/// debug build: in a release test build the call would start pkexec.
#[cfg(debug_assertions)]
#[test]
fn a_debug_build_never_starts_pkexec() {
    match spawn::pkexec(HASH) {
        Err(e) => assert!(e.contains("debug build"), "{e}"),
        Ok(_) => panic!("a debug build started pkexec"),
    }
}

/// Exit 0: the events in order, the check afterwards through after_fix,
/// and the sentence once that check agrees.
#[tokio::test]
async fn a_setup_that_ran_reports_its_events_in_order_and_checks_again() {
    let w = Fake::new().running(&applied_lines(run_lines(0, &["Complete!"])), exit(0));
    let (r, events) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Applied, "{r:#?}");
    assert_eq!(r.sentence, "Done. Models can use the graphics card now.");
    assert_eq!(w.spawned.lock().unwrap().as_slice(), [HASH]);
    assert_eq!(w.after_runs.load(Ordering::SeqCst), 1);
    assert_eq!(r.check.unwrap().verdict, MachineVerdict::Ready);
    assert_eq!(
        kinds(&events),
        [
            "password",
            "authorized",
            "started",
            "step_started",
            "line",
            "step_ended",
            "step_started",
            "line",
            "step_ended",
            "finished",
            "checking",
        ]
    );
    assert!(matches!(
        &events[3],
        MachineApplyEvent::StepStarted {
            step: 1,
            of: 2,
            kind: PlanStepKind::S1AddRepo
        }
    ));
    assert_eq!(
        r.output,
        [
            "creating /etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo",
            "Complete!"
        ]
    );
    assert!(r.detail.iter().any(|d| d.contains("asked by user 1000")));
    assert!(r
        .detail
        .iter()
        .any(|d| d.contains("CDI spec after the setup")));
    // The confirmation was the shell's plan.
    assert!(w.confirmations.lock().unwrap()[0]
        .text
        .contains("Install NVIDIA's tool"));
}

/// Exit 0 but the check afterwards still finds something: its headline.
#[tokio::test]
async fn a_setup_the_check_disagrees_with_says_what_it_found() {
    let w = Fake {
        after: check(
            MachineVerdict::NeedsRestart,
            "Restart the computer to finish the driver's update.",
            None,
        ),
        ..Fake::new().running(&applied_lines(run_lines(0, &[])), exit(0))
    };
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Applied);
    assert!(
        r.sentence.contains("still finds something"),
        "{}",
        r.sentence
    );
    assert!(r
        .sentence
        .ends_with("Restart the computer to finish the driver's update."));
}

#[tokio::test]
async fn a_plan_without_steps_was_nothing_to_do() {
    let w = Fake::new()
        .running(&[begun("apply"), Event::NothingToDo], exit(0))
        .checks(vec![fixable(), ready()]);
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::NothingToDo);
    assert_eq!(w.after_runs.load(Ordering::SeqCst), 0);
}

/// pkexec's own exits: 126 dismissed, 127 with and without "No
/// authentication agent found"; none of them checks again, and a 127 after
/// the helper's first line is the helper's, which never gives it.
#[tokio::test]
async fn pkexec_exits_map_as_the_table_says() {
    let cases = [
        (
            pkexec_says(
                126,
                "Error executing command as another user: Request dismissed\n",
            ),
            MachineApplyOutcome::Dismissed,
            "You closed the password window.",
        ),
        (
            pkexec_says(
                127,
                "Error executing command as another user: No authentication agent found.\n",
            ),
            MachineApplyOutcome::NoAgent,
            "no password window running",
        ),
        (
            pkexec_says(
                127,
                "Error executing command as another user: Not authorized\n\nThis incident has \
                 been reported.\n",
            ),
            MachineApplyOutcome::NotAuthorized,
            "The password wasn't accepted",
        ),
    ];
    for (ended, outcome, says) in cases {
        let w = Fake::new().running(&[], ended);
        let (r, events) = run(&w).await;
        assert_eq!(r.outcome, outcome, "{r:#?}");
        assert!(r.sentence.contains(says), "{}", r.sentence);
        assert!(r
            .detail
            .iter()
            .any(|d| d.starts_with("stderr: Error executing")));
        assert_eq!(w.after_runs.load(Ordering::SeqCst), 0);
        assert_eq!(kinds(&events), ["password"]);
    }
    let w = Fake::new().running(&[begun("apply")], exit(127));
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Failed);
    assert_eq!(w.after_runs.load(Ordering::SeqCst), 1);
}

/// pkexec's 127 with its own words about the program (D142): after the
/// password the exec failed, or before it the helper was not there. That is
/// no refused password: lmgw couldn't start the setup, or, with the program
/// replaced since lmgw started, lmgw was updated. Nothing ran as root.
#[tokio::test]
async fn pkexec_that_could_not_run_the_helper_did_not_refuse_the_password() {
    for stderr in [
        "Error executing /usr/libexec/lmgw/lmgw-machine-helper: No such file or directory\n",
        "Error accessing /usr/libexec/lmgw/lmgw-machine-helper: No such file or directory\n",
        "Cannot run program /usr/libexec/lmgw/lmgw-machine-helper: No such file or directory\n",
    ] {
        let w = Fake::new().running(&[], pkexec_says(127, stderr));
        let (r, _) = run(&w).await;
        assert_eq!(r.outcome, MachineApplyOutcome::Failed, "{stderr}: {r:#?}");
        assert_eq!(
            r.sentence,
            "lmgw couldn't start the setup. The details say why; nothing was changed."
        );
        assert!(r.detail.iter().any(|d| d.contains("No such file")));
        assert_eq!(w.after_runs.load(Ordering::SeqCst), 0);

        let w = Fake::new()
            .running(&[], pkexec_says(127, stderr))
            .updated_later();
        let (r, _) = run(&w).await;
        assert_eq!(r.outcome, MachineApplyOutcome::Updated, "{stderr}: {r:#?}");
        assert_eq!(
            r.sentence,
            "lmgw was updated while it was running. Restart lmgw, then try again."
        );
        assert!(r.detail.iter().any(|d| d.contains("(deleted)")));
        assert_eq!(w.after_runs.load(Ordering::SeqCst), 0);
    }
}

/// 101 (a panic), a signal, and a child that dies mid-run: stopped
/// unexpectedly, with what came so far, and the check afterwards.
#[tokio::test]
async fn a_crash_a_signal_or_a_dead_child_stopped_unexpectedly() {
    let mid_run = applied_lines(run_lines(0, &[]))[..5].to_vec();
    let killed = Ended {
        signal: Some(9),
        ..Ended::default()
    };
    for (lines, ended) in [
        (applied_lines(vec![]), exit(101)),
        (applied_lines(vec![]), killed.clone()),
        (mid_run, killed),
    ] {
        let w = Fake::new().running(&lines, ended);
        let (r, _) = run(&w).await;
        assert_eq!(r.outcome, MachineApplyOutcome::Failed, "{r:#?}");
        assert!(r
            .sentence
            .starts_with("Setting it up stopped unexpectedly."));
        assert_eq!(w.after_runs.load(Ordering::SeqCst), 1);
    }
    // Before any line pkexec ran nothing as root: no check afterwards.
    let w = Fake::new().running(&[], exit(1));
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Failed);
    assert_eq!(w.after_runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_refusal_and_a_failed_task_say_so() {
    let w = Fake::new().running(
        &[
            begun("apply"),
            Event::Refused {
                why: "this plan is for an image-based system".into(),
            },
        ],
        exit(4),
    );
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Refused);
    assert!(r
        .detail
        .iter()
        .any(|d| d.contains("this plan is for an image-based system")));
    assert_eq!(w.after_runs.load(Ordering::SeqCst), 0);

    let w = Fake::new().running(
        &[
            begun("apply"),
            Event::Note {
                line: "/usr/bin/systemd-run: Failed to start transient service unit".into(),
            },
            Event::TaskFailed {
                why: "systemd-run exited 1 before run began".into(),
            },
        ],
        exit(6),
    );
    let (r, events) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::TaskFailed);
    assert!(r
        .sentence
        .starts_with("Setting it up stopped unexpectedly."));
    assert!(r.output[0].contains("Failed to start transient service unit"));
    assert!(kinds(&events).contains(&"note"));
    assert_eq!(w.after_runs.load(Ordering::SeqCst), 1);
}

// --- Exit 2 (§3.5, D75, D88) ----------------------------------------------

/// Another version or schema: lmgw was updated; nothing is recomputed.
#[tokio::test]
async fn exit_2_from_another_version_or_schema_is_an_update() {
    for (version, schema) in [("0.0.1", PLAN_SCHEMA), (APP_VERSION, PLAN_SCHEMA + 1)] {
        let w = Fake::new().running(
            &[begun("apply"), plan_changed(version, schema, &[ROOT])],
            exit(2),
        );
        let (r, _) = run(&w).await;
        assert_eq!(r.outcome, MachineApplyOutcome::Updated, "{r:#?}");
        assert_eq!(
            r.sentence,
            "lmgw was updated while it was running. Restart lmgw, then try again."
        );
        assert_eq!(w.checks_run.load(Ordering::SeqCst), 1, "the gate's only");
        assert!(w.read_differently.lock().unwrap().is_empty());
    }
}

/// An update while the dialog was open (D143): the helper's build differs
/// only in its pipeline suffix, or not at all (a local build of the same
/// version) while `/usr/bin/lmgw` reads replaced. Either is an update,
/// never "read differently".
#[tokio::test]
async fn exit_2_after_an_update_while_the_dialog_was_open_is_an_update() {
    let w = Fake::new().running(
        &[
            begun("apply"),
            plan_changed(&format!("{APP_VERSION}+99"), PLAN_SCHEMA, &[ROOT]),
        ],
        exit(2),
    );
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Updated, "{r:#?}");
    assert!(r.detail.iter().any(|d| d.contains("+99")), "{:?}", r.detail);

    let w = Fake::new()
        .running(
            &[
                begun("apply"),
                plan_changed(APP_VERSION, PLAN_SCHEMA, &[ROOT]),
            ],
            exit(2),
        )
        .updated_later();
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Updated, "{r:#?}");
    assert_eq!(
        r.sentence,
        "lmgw was updated while it was running. Restart lmgw, then try again."
    );
    assert!(r.detail.iter().any(|d| d.contains("(deleted)")));
    assert_eq!(w.checks_run.load(Ordering::SeqCst), 1, "the gate's only");
    assert!(w.read_differently.lock().unwrap().is_empty());
}

/// The same version, and the plan computed again is one of root's: the
/// computer changed, here is the new list.
#[tokio::test]
async fn exit_2_with_a_new_plan_root_also_computes_offers_the_new_list() {
    let w = Fake::new()
        .running(
            &[
                begun("apply"),
                plan_changed(APP_VERSION, PLAN_SCHEMA, &[OTHER, ROOT]),
            ],
            exit(2),
        )
        .checks(vec![
            fixable(),
            check(
                MachineVerdict::NeedsFix,
                "…",
                Some(plan(OTHER, PlanHow::Helper)),
            ),
        ]);
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::PlanChanged, "{r:#?}");
    assert_eq!(
        r.sentence,
        "Something on this computer changed since you confirmed. Here is the new list."
    );
    assert_eq!(r.check.unwrap().plan.unwrap().hash, OTHER);
    assert!(w.read_differently.lock().unwrap().is_empty());
}

/// The same version, and the plan computed again is none of root's: the
/// button stops for that plan (recorded with core), both plans are in the
/// detail, and the check after that is the answer's.
#[tokio::test]
async fn exit_2_with_a_plan_root_does_not_compute_stops_the_button() {
    let instructions = check(
        MachineVerdict::NeedsSteps,
        "…",
        Some(plan(
            HASH,
            PlanHow::Instructions {
                why: lmgw_machine::plan::instructions::READ_DIFFERENTLY.into(),
            },
        )),
    );
    let w = Fake::new()
        .running(
            &[
                begun("apply"),
                plan_changed(APP_VERSION, PLAN_SCHEMA, &[ROOT]),
            ],
            exit(2),
        )
        .checks(vec![fixable(), fixable(), instructions]);
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::ReadDifferently, "{r:#?}");
    assert_eq!(
        r.sentence,
        "lmgw and your system read this computer differently, so lmgw can't set it up by \
         itself."
    );
    assert_eq!(w.read_differently.lock().unwrap().as_slice(), [HASH]);
    assert!(r
        .detail
        .iter()
        .any(|d| d == &format!("lmgw's plan (sha256 {HASH}):")));
    assert!(r.detail.iter().any(|d| d.starts_with(&format!(
        "the system's plan without the toolkit's update (sha256 {ROOT}"
    ))));
    assert!(r
        .detail
        .iter()
        .any(|d| d.contains("creates /etc/yum.repos.d/")));
    let after = r.check.unwrap().plan.unwrap();
    assert!(matches!(after.how, PlanHow::Instructions { .. }));
    assert_eq!(w.after_runs.load(Ordering::SeqCst), 0);
}

// --- A setup already running (exit 5) ------------------------------------

fn entry(n: usize, e: Event) -> Entry {
    Entry {
        cursor: format!("s=x;i={n}"),
        event: e.to_line(),
    }
}

/// The helper's exit 5: lmgw follows the unit through systemd and its
/// journal, then checks; the journal's `finished` says how it ended.
#[tokio::test]
async fn exit_5_follows_the_running_unit_to_its_end() {
    let journal = run_lines(0, &["Complete!"]);
    let w = Fake::new()
        .running(
            &[
                begun("apply"),
                Event::AlreadyRunning {
                    state: "active".into(),
                },
            ],
            exit(5),
        )
        .states(&["inactive", "active", "active", "inactive"]);
    *w.journal.lock().unwrap() = VecDeque::from([
        Ok(journal[..4]
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, e)| entry(i, e))
            .collect()),
        Ok(vec![]),
        Ok(journal[4..]
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, e)| entry(i + 4, e))
            .collect()),
    ]);
    let (r, events) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Applied, "{r:#?}");
    assert_eq!(w.after_runs.load(Ordering::SeqCst), 1);
    let k = kinds(&events);
    assert_eq!(&k[..3], ["password", "authorized", "following"]);
    assert!(k.contains(&"finished") && k.ends_with(&["checking"]));
    let asks = w.journal_asks.lock().unwrap().clone();
    assert_eq!(asks[0], ("0123abcd".to_string(), None));
    assert_eq!(
        asks[1].1.as_deref(),
        Some("s=x;i=3"),
        "after the last cursor"
    );
}

/// A unit found running before the password: followed, with no dialog and
/// no pkexec; a journal it may not read leaves the check to say how it
/// went.
#[tokio::test]
async fn a_unit_already_running_is_followed_without_a_password() {
    let w = Fake::new().states(&["activating", "active", "inactive"]);
    *w.journal.lock().unwrap() = VecDeque::from([Err("journalctl: permission".to_string())]);
    let (r, events) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Ended, "{r:#?}");
    assert_eq!((w.asked(), w.spawned()), (0, 0));
    assert_eq!(
        kinds(&events),
        ["following", "checking"],
        "nothing from a journal it cannot read"
    );
    assert!(r.detail.iter().any(|d| d.contains("could not be read")));
    assert_eq!(w.after_runs.load(Ordering::SeqCst), 1);

    // Without an invocation the journal is not asked at all; systemd
    // failing ends the following too.
    let w = Fake {
        invocation: None,
        ..Fake::new()
    };
    *w.states.lock().unwrap() =
        VecDeque::from([Ok("active".to_string()), Err("no bus".to_string())]);
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::Ended);
    assert!(r.sentence.contains("couldn't see how"), "{}", r.sentence);
    assert!(w.journal_asks.lock().unwrap().is_empty());
}

/// A followed unit whose journal says a step failed: the plain reason,
/// from the step's own words (no plan sentence after a restart).
#[tokio::test]
async fn a_followed_unit_that_failed_says_why() {
    let journal = run_lines(1, &["Curl error (28): Timeout was reached"]);
    let w = Fake::new().states(&["active", "inactive"]);
    *w.journal.lock().unwrap() = VecDeque::from([Ok(journal
        .into_iter()
        .enumerate()
        .map(|(i, e)| entry(i, e))
        .collect())]);
    let (r, _) = run(&w).await;
    assert_eq!(r.outcome, MachineApplyOutcome::StepFailed, "{r:#?}");
    assert!(r
        .sentence
        .contains("NVIDIA's download server didn't answer"));
}

/// The journal is read by trusted fields alone: journalctl's matches, and
/// every entry it prints checked again.
#[test]
fn the_journal_is_read_by_trusted_fields_only() {
    let args = follow::journal_args("0123abcd", Some("s=1"));
    assert!(args.contains(&"_SYSTEMD_UNIT=lmgw-machine-setup.service".to_string()));
    assert!(args.contains(&"_UID=0".to_string()));
    assert!(args.contains(&"_SYSTEMD_INVOCATION_ID=0123abcd".to_string()));
    assert!(args.contains(&"--after-cursor=s=1".to_string()));
    assert!(
        !args
            .iter()
            .any(|a| a.contains("SYSLOG_IDENTIFIER") || a == "-t" || a.starts_with("LMGW_EVENT=")),
        "{args:?}"
    );

    let ev = Event::Finished { exit: 0 }.to_line();
    let row = |unit: &str, uid: &str, inv: &str| {
        serde_json::json!({
            "__CURSOR": "c",
            "_SYSTEMD_UNIT": unit,
            "_UID": uid,
            "_SYSTEMD_INVOCATION_ID": inv,
            "LMGW_EVENT": ev,
            "SYSLOG_IDENTIFIER": "lmgw-machine-setup",
        })
        .to_string()
    };
    let text = [
        row("lmgw-machine-setup.service", "0", "0123abcd"),
        // Anyone's entry with the identifier and an event of their choosing.
        row("app-kde-1234.scope", "1000", "0123abcd"),
        row("lmgw-machine-setup.service", "1000", "0123abcd"),
        // An earlier run's.
        row("lmgw-machine-setup.service", "0", "ffff"),
        serde_json::json!({"__CURSOR": "d", "SYSLOG_IDENTIFIER": "lmgw-machine-setup", "LMGW_EVENT": ev}).to_string(),
        // A value journalctl prints as bytes.
        serde_json::json!({
            "__CURSOR": "e",
            "_SYSTEMD_UNIT": "lmgw-machine-setup.service",
            "_UID": "0",
            "_SYSTEMD_INVOCATION_ID": "0123abcd",
            "LMGW_EVENT": ev.as_bytes(),
        })
        .to_string(),
        "not json".to_string(),
    ]
    .join("\n");
    let got = follow::parse_journal(&text, "0123abcd");
    assert_eq!(
        got,
        [
            Entry {
                cursor: "c".into(),
                event: ev.clone()
            },
            Entry {
                cursor: "e".into(),
                event: ev
            }
        ]
    );
}

// --- Lines are text (WP6's review) -----------------------------------------

/// A line's text goes on byte for byte, markup and control characters
/// included: nothing here turns it into anything but a JSON string, and
/// the page shows it as text.
#[tokio::test]
async fn a_line_is_passed_on_as_text_exactly() {
    let nasty = "<img src=x onerror=alert(1)> & \"quoted\" 'single' \u{1b}[1mbold\u{1b}[0m \\ ü";
    let w = Fake::new().running(
        &applied_lines(vec![
            begun("run"),
            Event::StepStarted {
                step: 1,
                of: 1,
                kind: StepKind::Install,
            },
            line(1, StepKind::Install, Stream::Stdout, nasty),
            step_ended(1, StepKind::Install, 0),
            Event::Finished { exit: 0 },
        ]),
        exit(0),
    );
    let (r, events) = run(&w).await;
    let sent = events
        .iter()
        .find_map(|e| match e {
            MachineApplyEvent::Line { line, stream, .. } => Some((line.clone(), *stream)),
            _ => None,
        })
        .unwrap();
    assert_eq!(sent, (nasty.to_string(), MachineLineStream::Stdout));
    assert_eq!(r.output, [nasty]);
    // On the wire: a JSON string field, which parses back to the same text.
    let wire = serde_json::to_string(&MachineApplyEvent::Line {
        step: 1,
        kind: PlanStepKind::S4Install,
        stream: MachineLineStream::Stdout,
        line: nasty.into(),
    })
    .unwrap();
    assert!(wire
        .starts_with(r#"{"event":"line","step":1,"kind":"s4_install","stream":"stdout","line":"#));
    let back: MachineApplyEvent = serde_json::from_str(&wire).unwrap();
    assert!(matches!(back, MachineApplyEvent::Line { line, .. } if line == nasty));
    // A line that is no event is said as a note, not dropped.
    let mut p = super::progress::Progress::default();
    let sink = Events::default();
    p.line("<b>not json</b>", &sink);
    assert_eq!(p.output, ["the setup printed: <b>not json</b>"]);
    assert!(matches!(
        &sink.all()[..],
        [MachineApplyEvent::Note { line }] if line == "the setup printed: <b>not json</b>"
    ));
}

/// The event names WP8a reads (§4.1).
#[test]
fn the_events_serialize_with_their_names() {
    let names: Vec<String> = [
        MachineApplyEvent::Password,
        MachineApplyEvent::Authorized,
        MachineApplyEvent::Following {
            state: "active".into(),
        },
        MachineApplyEvent::Started { steps: 2 },
        MachineApplyEvent::Finished { exit: 0 },
        MachineApplyEvent::Checking,
    ]
    .iter()
    .map(|e| {
        serde_json::to_value(e).unwrap()["event"]
            .as_str()
            .unwrap()
            .to_string()
    })
    .collect();
    assert_eq!(
        names,
        [
            "password",
            "authorized",
            "following",
            "started",
            "finished",
            "checking"
        ]
    );
    assert_eq!(
        serde_json::to_value(MachineApplyOutcome::ReadDifferently).unwrap(),
        "read_differently"
    );
}
