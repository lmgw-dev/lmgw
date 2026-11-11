//! The machine check's fix (machine-check design §3.4, §3.5, §4.3, §4.5,
//! WP7): the shell command `machine_apply`, which runs the root helper
//! after the shell's own confirmation and one password prompt.
//!
//! The order of one call:
//!
//! 1. **The gates** ([`gates`], §3.4): the invoke handler in `main` already
//!    answered only the gateway's own document in the app's window
//!    (`media::command_allowed`); then a release build, not a dev instance,
//!    the installed `/usr/bin/lmgw` on the installed data dir, and the
//!    helper installed and sane with `/usr/bin/pkexec` at that path. A
//!    refused gate starts nothing and asks nothing.
//! 2. **A setup already running is followed** ([`follow`]), never a second
//!    one started: systemd is asked first.
//! 3. **A fresh check**, in-process: its plan must be the one the page
//!    showed (the hash the command was called with), and offered as the
//!    button (`how` = `helper`, which only a dnf5 system with the helper,
//!    no AppImage, no other repository offering the toolkit and no plan the
//!    helper read differently gets). Otherwise the answer is the new check
//!    and why, without a password.
//! 4. **The confirmation** ([`confirm`], §4.5, D25), drawn by the shell
//!    from that plan, only over the app's window in front, "Set it up"
//!    pressable a second after it got the focus, and never again for a
//!    document that said "Not now" (D133, D134, D140). "Not now" is the
//!    default; only "Set it up" goes on.
//! 5. **The helper** ([`spawn`]): `/usr/bin/pkexec --disable-internal-agent
//!    /usr/libexec/lmgw/lmgw-machine-helper apply <hash>`. Its lines become
//!    the `Channel`'s events as they come ([`progress`]); its first `begun`
//!    line ends the password phase. The hash it gets is the fresh check's
//!    (equal to the page's by then), lmgw's own value.
//! 6. **Its end** ([`outcome`], §3.5's table), and **afterwards** a fresh
//!    check through core with lmgw's own spec written first and the
//!    ledger's probe installed again where it read nothing
//!    (`lmgw_core::machine::after_fix`). An exit 2 is answered by checking
//!    again (D88), after asking whether lmgw itself was updated meanwhile
//!    (D143).
//!
//! Everything the command touches outside itself goes through [`World`],
//! so the tests state the gates, the check, the dialog, the helper's lines
//! and systemd's answers, and nothing of this computer is read or run. Only
//! [`machine_apply`] builds the real one ([`Real`]), and its spawn refuses
//! in a debug build as well.

/// The audit log's entry for a call (headless server design §5.8).
mod audited;
// The confirmation window is drawn on Linux only so far; its rules and
// tests are portable.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod confirm;
mod follow;
mod gates;
mod outcome;
mod progress;
mod reason;
mod spawn;
#[cfg(test)]
mod tests;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};

use lmgw_api_types::machine::{
    MachineApplyEvent, MachineApplyOutcome, MachineApplyResult, MachineCheck, MachinePlan,
    MachineVerdict, PlanHow, PlanStepKind,
};
use lmgw_core::state::SharedState;
use lmgw_machine::helper::{unit_running, UNIT_SERVICE};
use lmgw_machine::plan::PLAN_SCHEMA;

use confirm::{Answer, Asked, Confirmation};
use follow::{Entry, Followed};
use gates::Host;
use outcome::Ending;
use progress::{PlanChanged, Progress};
use spawn::Helper;

/// The lmgw build this shell is: compared with the helper's on an exit 2
/// (D75). Both carry the workspace version, which `ci/build.sh` stamps as
/// `<X.Y.Z>+<pipeline>` for a test-feed RPM before either is built, so two
/// such builds differ here too (D143).
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A boxed future, as the world's methods return them.
pub(crate) type Erased<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where the command's events go: the `Channel` the page passed.
pub(crate) trait Sink: Send + Sync {
    fn send(&self, event: MachineApplyEvent);
}

impl Sink for tauri::ipc::Channel<MachineApplyEvent> {
    fn send(&self, event: MachineApplyEvent) {
        if let Err(e) = tauri::ipc::Channel::send(self, event) {
            tracing::debug!("machine_apply: an event did not reach the page: {e}");
        }
    }
}

/// Everything outside the command: the gates' facts, systemd, the journal,
/// the check, the dialog and the helper.
pub(crate) trait World: Send + Sync {
    /// The process the gates judge.
    fn host(&self) -> Host;
    /// `lmgw_machine::plan::instructions::helper_installed` on this machine.
    fn helper_installed(&self) -> Result<(), String>;
    /// systemd's `ActiveState` for the unit (`unit_state_args`).
    fn unit_state(&self) -> Erased<'_, Result<String, String>>;
    /// The running unit's `InvocationID`, when systemd names one.
    fn invocation(&self) -> Erased<'_, Option<String>>;
    /// The unit's journal entries of `invocation`, after `cursor`, by
    /// trusted fields only ([`follow`]).
    fn journal<'a>(
        &'a self,
        invocation: &'a str,
        cursor: Option<&'a str>,
    ) -> Erased<'a, Result<Vec<Entry>, String>>;
    /// The wait between two looks at a running unit ([`follow::FOLLOW_POLL`]).
    fn pause(&self) -> Erased<'_, ()>;
    /// A fresh machine check.
    fn check(&self) -> Erased<'_, MachineCheck>;
    /// After the helper ran: lmgw's own spec, a fresh check and the probe
    /// (`lmgw_core::machine::after_fix`); the second half is for the detail.
    fn after_fix(&self) -> Erased<'_, (MachineCheck, Vec<String>)>;
    /// The helper read the plan `hash` differently (D88): never offer it
    /// again.
    fn read_differently(&self, hash: &str);
    /// The shell's own confirmation, through [`confirm::Asked`]'s rules:
    /// what the person answered, or why no dialog was shown.
    fn confirm(&self, c: Confirmation) -> Erased<'_, Answer>;
    /// Start pkexec and the helper for the plan `hash`.
    fn spawn(&self, hash: &str) -> Result<Box<dyn Helper>, String>;
}

/// One setup at a time from this process: a second call while one runs is
/// answered at once, without a dialog.
static BUSY: AtomicBool = AtomicBool::new(false);

/// The confirmation's memory across calls (the document that said "Not
/// now", D140).
static ASKED: Asked = Asked::new();

/// Holds [`BUSY`] (or a test's flag) for one call.
struct Once<'a>(&'a AtomicBool);

impl<'a> Once<'a> {
    fn take(flag: &'a AtomicBool) -> Option<Self> {
        (!flag.swap(true, Ordering::SeqCst)).then_some(Self(flag))
    }
}

impl Drop for Once<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// `machine_apply` (§4.3): set up what the check's plan `hash` lists, after
/// the shell's confirmation and the password, reporting progress on
/// `on_event`. The page passes `{ hash, onEvent }`; the answer says how it
/// ended and carries the check made afterwards. WP8a's `MachineCheck`
/// component is its caller.
#[tauri::command]
pub async fn machine_apply(
    app: tauri::AppHandle,
    hash: String,
    on_event: tauri::ipc::Channel<MachineApplyEvent>,
) -> Result<MachineApplyResult, String> {
    use tauri::Manager;
    let Some(state) = app.try_state::<SharedState>() else {
        return Err("lmgw is still starting".into());
    };
    let world = Real {
        app: app.clone(),
        state: state.inner().clone(),
    };
    let result = guarded(&BUSY, &world, &hash, &on_event).await;
    audited::record(state.inner(), &hash, &result).await;
    Ok(result)
}

/// [`apply`], once at a time per `flag`.
async fn guarded(
    flag: &AtomicBool,
    world: &dyn World,
    hash: &str,
    sink: &dyn Sink,
) -> MachineApplyResult {
    let Some(_once) = Once::take(flag) else {
        return answer(
            MachineApplyOutcome::Unavailable,
            "lmgw is already setting this up. Its progress shows where it was started.",
            None,
            Progress::default(),
            None,
        );
    };
    apply(world, hash, sink).await
}

/// The answer, with what the lines said.
fn answer(
    outcome: MachineApplyOutcome,
    sentence: impl Into<String>,
    step: Option<PlanStepKind>,
    p: Progress,
    check: Option<MachineCheck>,
) -> MachineApplyResult {
    let step = step.or(p.last_step.map(|(_, k)| k));
    MachineApplyResult {
        outcome,
        sentence: sentence.into(),
        step,
        output: p.output,
        detail: p.detail,
        check,
    }
}

/// One call, after the origin check (see the module docs for the order).
pub(crate) async fn apply(world: &dyn World, hash: &str, sink: &dyn Sink) -> MachineApplyResult {
    let mut p = Progress::default();
    if let Some(r) = gates::refuse(&world.host(), world.helper_installed()) {
        p.detail = r.detail;
        return answer(MachineApplyOutcome::Unavailable, r.sentence, None, p, None);
    }
    match world.unit_state().await {
        Ok(state) if unit_running(&state) => return followed(world, sink, state, p, None).await,
        Ok(_) => {}
        Err(why) => p.detail.push(format!(
            "systemd could not say whether {UNIT_SERVICE} runs ({why}); the helper asks again"
        )),
    }
    let check = world.check().await;
    let plan = match &check.plan {
        None => {
            let sentence = format!("There's nothing left to set up. {}", check.headline);
            return answer(
                MachineApplyOutcome::NothingToDo,
                sentence,
                None,
                p,
                Some(check),
            );
        }
        Some(plan) if plan.hash != hash => {
            p.detail.push(format!(
                "the page's plan is {hash}; lmgw's check computes {} now",
                plan.hash
            ));
            return answer(
                MachineApplyOutcome::PlanChanged,
                "Something on this computer changed since lmgw showed the list. Here is the new \
                 list; nothing was changed.",
                None,
                p,
                Some(check),
            );
        }
        Some(MachinePlan {
            how: PlanHow::Instructions { why },
            ..
        }) => {
            let why = why.clone();
            return answer(MachineApplyOutcome::Unavailable, why, None, p, Some(check));
        }
        Some(plan) => plan.clone(),
    };
    match world.confirm(confirm::of(&plan)).await {
        Answer::SetItUp => {}
        Answer::NotNow => {
            return answer(
                MachineApplyOutcome::Declined,
                "Nothing was changed.",
                None,
                p,
                Some(check),
            )
        }
        Answer::DeclinedBefore => {
            p.detail.push(
                "this document answered \"Not now\" already; it is asked again once the window \
                 loads a page anew"
                    .into(),
            );
            return answer(
                MachineApplyOutcome::Declined,
                "Nothing was changed: you chose \"Not now\" already. Reload the page to be asked \
                 again.",
                None,
                p,
                Some(check),
            );
        }
        Answer::NotShown(why) => {
            p.detail.push(format!("no confirmation was shown: {why}"));
            return answer(
                MachineApplyOutcome::Unavailable,
                "Open lmgw's window and press Set it up there.",
                None,
                p,
                Some(check),
            );
        }
    }
    sink.send(MachineApplyEvent::Password);
    let mut helper = match world.spawn(&plan.hash) {
        Ok(h) => h,
        Err(e) => {
            p.detail.push(e);
            return answer(
                MachineApplyOutcome::Failed,
                "lmgw couldn't start the setup. The details say why; nothing was changed.",
                None,
                p,
                Some(check),
            );
        }
    };
    while let Some(line) = helper.line().await {
        p.line(&line, sink);
    }
    let ended = helper.ended().await;
    p.detail.extend(outcome::ended_detail(&ended));
    let ending = outcome::ending(&ended, &p);
    ended_as(world, sink, ending, p, &plan, check).await
}

/// The answer for `ending`, after checking again where the helper may have
/// changed something.
async fn ended_as(
    world: &dyn World,
    sink: &dyn Sink,
    ending: Ending,
    mut p: Progress,
    plan: &MachinePlan,
    before: MachineCheck,
) -> MachineApplyResult {
    use MachineApplyOutcome as O;
    match ending {
        Ending::Done => {
            let check = fixed(world, sink, &mut p).await;
            let sentence = applied(&check);
            answer(O::Applied, sentence, None, p, Some(check))
        }
        Ending::NothingToDo => {
            sink.send(MachineApplyEvent::Checking);
            let check = world.check().await;
            let sentence = format!("There was nothing left to set up. {}", check.headline);
            answer(O::NothingToDo, sentence, None, p, Some(check))
        }
        Ending::PlanChanged(pc) => changed(world, sink, pc, p, before).await,
        Ending::StepFailed => {
            let check = fixed(world, sink, &mut p).await;
            step_failed(p, Some(plan), check)
        }
        Ending::Refused(why) => {
            p.detail.push(format!("the helper's reason: {why}"));
            answer(
                O::Refused,
                "lmgw couldn't start the setup. The details say why; nothing was changed.",
                None,
                p,
                Some(before),
            )
        }
        Ending::AlreadyRunning(state) => followed(world, sink, state, p, Some(plan)).await,
        Ending::TaskFailed(why) => {
            p.detail.push(format!("the system task: {why}"));
            let check = fixed(world, sink, &mut p).await;
            answer(O::TaskFailed, STOPPED, None, p, Some(check))
        }
        Ending::Dismissed => answer(
            O::Dismissed,
            "You closed the password window. Nothing was changed.",
            None,
            p,
            Some(before),
        ),
        Ending::NoAgent => answer(
            O::NoAgent,
            "Your desktop has no password window running, so lmgw can't ask for the password. \
             Here are the commands to run in a terminal instead.",
            None,
            p,
            Some(before),
        ),
        Ending::NotRun => match gates::updated(&world.host()) {
            Some(why) => {
                p.detail.push(why);
                answer(O::Updated, UPDATED, None, p, Some(before))
            }
            None => {
                p.detail
                    .push("pkexec could not run the setup helper; nothing ran as root".into());
                answer(
                    O::Failed,
                    "lmgw couldn't start the setup. The details say why; nothing was changed.",
                    None,
                    p,
                    Some(before),
                )
            }
        },
        Ending::NotAuthorized => answer(
            O::NotAuthorized,
            "The password wasn't accepted, or this account may not change the system. Someone \
             who can has to type their password.",
            None,
            p,
            Some(before),
        ),
        Ending::Failed(why) => {
            p.detail.push(why);
            // Before the helper's first line nothing ran as root.
            let check = if p.authorized {
                fixed(world, sink, &mut p).await
            } else {
                before
            };
            answer(O::Failed, STOPPED, None, p, Some(check))
        }
    }
}

/// §3.5's sentence for an lmgw updated while it ran.
const UPDATED: &str = "lmgw was updated while it was running. Restart lmgw, then try again.";

/// §3.5's sentence for a setup that stopped unexpectedly.
const STOPPED: &str = "Setting it up stopped unexpectedly. The output so far is in the details, \
                       and the check shows what is in place now.";

/// After the helper's steps: `after_fix`, its words in the detail.
async fn fixed(world: &dyn World, sink: &dyn Sink, p: &mut Progress) -> MachineCheck {
    sink.send(MachineApplyEvent::Checking);
    let (check, said) = world.after_fix().await;
    p.detail.extend(said);
    check
}

/// Exit 0's sentence, once the check afterwards agrees (§3.5).
fn applied(check: &MachineCheck) -> String {
    if check.verdict == MachineVerdict::Ready {
        "Done. Models can use the graphics card now.".into()
    } else {
        format!(
            "lmgw made the changes, but the check afterwards still finds something to do. {}",
            check.headline
        )
    }
}

/// Exit 3's answer: the step and its plain reason.
fn step_failed(p: Progress, plan: Option<&MachinePlan>, check: MachineCheck) -> MachineApplyResult {
    let failed = p.failed.clone().or_else(|| {
        p.last_step.map(|(step, kind)| progress::FailedStep {
            step,
            kind,
            exit: None,
            signal: None,
            error: None,
        })
    });
    let Some(failed) = failed else {
        return answer(
            MachineApplyOutcome::StepFailed,
            "Setting it up didn't finish. The output is in the details, and the check shows what \
             is in place now.",
            None,
            p,
            Some(check),
        );
    };
    let lines: Vec<&str> = p
        .step_lines
        .iter()
        .filter(|(s, _)| *s == failed.step)
        .map(|(_, l)| l.as_str())
        .collect();
    let sentence = plan
        .and_then(|plan| plan.steps.get(failed.step.wrapping_sub(1)))
        .filter(|s| s.kind == failed.kind)
        .map(|s| s.sentence.as_str());
    let reason = reason::plain(&failed, &lines, sentence);
    let kind = failed.kind;
    answer(
        MachineApplyOutcome::StepFailed,
        format!(
            "Setting it up didn't finish. {reason} The step's output is in the details, and the \
             steps before it stay done."
        ),
        Some(kind),
        p,
        Some(check),
    )
}

/// Exit 2 (§3.5, D75, D88): lmgw updated, the computer changed, or lmgw
/// and the system reading this computer differently.
async fn changed(
    world: &dyn World,
    sink: &dyn Sink,
    pc: PlanChanged,
    mut p: Progress,
    before: MachineCheck,
) -> MachineApplyResult {
    // An update while the dialog was open replaced the program too, and a
    // build of the same version reads alike in the line (D143).
    let replaced = gates::updated(&world.host());
    if pc.schema != PLAN_SCHEMA || pc.version != APP_VERSION || replaced.is_some() {
        p.detail.push(format!(
            "the helper is lmgw {} with plan schema {}; this app is lmgw {APP_VERSION} with plan \
             schema {PLAN_SCHEMA}",
            pc.version, pc.schema
        ));
        p.detail.extend(replaced);
        return answer(MachineApplyOutcome::Updated, UPDATED, None, p, Some(before));
    }
    sink.send(MachineApplyEvent::Checking);
    let check = world.check().await;
    let root: Vec<&str> = pc.plans.iter().map(|v| v.sha256.as_str()).collect();
    match &check.plan {
        Some(plan) if plan.how == PlanHow::Helper && !root.contains(&plan.hash.as_str()) => {
            world.read_differently(&plan.hash);
            p.detail.push(format!(
                "lmgw computes the plan {} again, which is none of the system's ({}): the \
                 button is not offered for it again",
                plan.hash,
                root.join(", ")
            ));
            p.detail.extend(outcome::our_plan(plan));
            p.detail.extend(outcome::root_plans(&pc.plans));
            let again = world.check().await;
            answer(
                MachineApplyOutcome::ReadDifferently,
                "lmgw and your system read this computer differently, so lmgw can't set it up \
                 by itself.",
                None,
                p,
                Some(again),
            )
        }
        _ => answer(
            MachineApplyOutcome::PlanChanged,
            "Something on this computer changed since you confirmed. Here is the new list.",
            None,
            p,
            Some(check),
        ),
    }
}

/// Follow a setup that runs already (exit 5, or found before the
/// password) to its end, then check (§3.5).
async fn followed(
    world: &dyn World,
    sink: &dyn Sink,
    state: String,
    mut p: Progress,
    plan: Option<&MachinePlan>,
) -> MachineApplyResult {
    let how = follow::follow(world, sink, state, &mut p).await;
    let check = fixed(world, sink, &mut p).await;
    match (how, p.finished) {
        (Followed::Ended, Some(0)) => {
            let sentence = applied(&check);
            answer(MachineApplyOutcome::Applied, sentence, None, p, Some(check))
        }
        (Followed::Ended, Some(3)) => step_failed(p, plan, check),
        (Followed::Lost(_), _) => answer(
            MachineApplyOutcome::Ended,
            "lmgw couldn't see how the setup that was running ends. The check shows what is in \
             place now.",
            None,
            p,
            Some(check),
        ),
        _ => answer(
            MachineApplyOutcome::Ended,
            "The setup that was running has ended. The check shows what is in place now.",
            None,
            p,
            Some(check),
        ),
    }
}

/// This computer, the app's state and its window.
struct Real {
    app: tauri::AppHandle,
    state: SharedState,
}

/// Run a system tool by its absolute path, as the user: its stdout, or why
/// it gave none. systemd's and journald's own timeouts bound it.
async fn run_tool(program: &str, args: &[String]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .env_clear()
        .env("LC_ALL", "C.UTF-8")
        .kill_on_drop(true);
    lmgw_core::child::system_tool_tokio(&mut cmd);
    let out = cmd
        .output()
        .await
        .map_err(|e| format!("{program} could not be run: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl World for Real {
    fn host(&self) -> Host {
        Host::this(&self.state)
    }

    fn helper_installed(&self) -> Result<(), String> {
        lmgw_machine::plan::instructions::helper_installed(&lmgw_machine::SystemFacts::new())
    }

    fn unit_state(&self) -> Erased<'_, Result<String, String>> {
        Box::pin(async move {
            let args = lmgw_machine::helper::unit_state_args().map(String::from);
            run_tool(follow::SYSTEMCTL, &args)
                .await
                .map(|s| s.trim().to_string())
        })
    }

    fn invocation(&self) -> Erased<'_, Option<String>> {
        Box::pin(async move {
            let args = follow::invocation_args().map(String::from);
            run_tool(follow::SYSTEMCTL, &args)
                .await
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
    }

    fn journal<'a>(
        &'a self,
        invocation: &'a str,
        cursor: Option<&'a str>,
    ) -> Erased<'a, Result<Vec<Entry>, String>> {
        Box::pin(async move {
            let text = run_tool(
                follow::JOURNALCTL,
                &follow::journal_args(invocation, cursor),
            )
            .await?;
            Ok(follow::parse_journal(&text, invocation))
        })
    }

    fn pause(&self) -> Erased<'_, ()> {
        Box::pin(tokio::time::sleep(follow::FOLLOW_POLL))
    }

    fn check(&self) -> Erased<'_, MachineCheck> {
        Box::pin(async move { (*lmgw_core::machine::check(&self.state, true).await).clone() })
    }

    fn after_fix(&self) -> Erased<'_, (MachineCheck, Vec<String>)> {
        Box::pin(async move {
            let (check, fixed) =
                lmgw_core::machine::after_fix(&self.state, lmgw_core::vram::detect_probe).await;
            let mut said = vec![fixed.spec];
            if fixed.probe_installed {
                said.push(
                    "the ledger reads the graphics card now (its probe was installed again)".into(),
                );
            }
            ((*check).clone(), said)
        })
    }

    fn read_differently(&self, hash: &str) {
        self.state.machine.read_differently(hash);
    }

    fn confirm(&self, c: Confirmation) -> Erased<'_, Answer> {
        Box::pin(async move {
            #[cfg(target_os = "linux")]
            {
                confirm::ask(&self.app, c, &ASKED).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (c, &self.app, &ASKED);
                Answer::NotShown("the confirmation exists on Linux only")
            }
        })
    }

    fn spawn(&self, hash: &str) -> Result<Box<dyn Helper>, String> {
        spawn::pkexec(hash)
    }
}
