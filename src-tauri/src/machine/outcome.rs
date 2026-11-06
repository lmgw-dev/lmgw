//! How the helper ended, read from its exit code and its lines
//! (machine-check design §3.5's table), and the plans of an exit 2 shown
//! in full.

use lmgw_api_types::machine::MachinePlan;
use lmgw_machine::helper::{Exit, PlanVariant};
use lmgw_machine::plan::instructions::command_line;

use super::progress::{PlanChanged, Progress};
use super::spawn::Ended;

/// What the helper's end means, before the command checks again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ending {
    /// Exit 0: every step ran.
    Done,
    /// Exit 0 with `nothing_to_do`: the plan had no steps.
    NothingToDo,
    /// Exit 2 with its `plan_changed` line.
    PlanChanged(PlanChanged),
    /// Exit 3: a step failed.
    StepFailed,
    /// Exit 4, and the helper's reason.
    Refused(String),
    /// Exit 5: the system task runs already; systemd's word for it.
    AlreadyRunning(String),
    /// Exit 6, and what the helper saw.
    TaskFailed(String),
    /// pkexec's 126: the password window was closed.
    Dismissed,
    /// pkexec's 127, and no password window on the desktop.
    NoAgent,
    /// pkexec's 127 otherwise.
    NotAuthorized,
    /// pkexec's 127 with its own words about the program it was to run
    /// (WP7 review L3, D142): the helper could not be run, as when an
    /// update replaced it between the gates and the spawn. Nothing ran as
    /// root.
    NotRun,
    /// Anything else (101, a signal, a code nothing gives), and what it
    /// was, for the detail.
    Failed(String),
}

/// pkexec's words when the desktop has no password window running
/// (`--disable-internal-agent`, D120), and the internal agent's, should a
/// pkexec ever try it anyway.
fn no_agent(stderr: &str) -> bool {
    stderr.contains("No authentication agent found")
        || stderr.contains("Error creating textual authentication agent")
}

/// pkexec's words when it could not run the program itself (polkit 127's
/// strings): "Error accessing <program>: …" and "Cannot run program
/// <program>: …" before the password, "Error executing <program>: …" after
/// it (the exec failed). Its words about the authorization all read "Error
/// executing command as another user: …", which these never do.
fn not_run(stderr: &str) -> bool {
    !stderr.contains("as another user")
        && [
            "Error executing ",
            "Error accessing ",
            "Cannot run program ",
        ]
        .iter()
        .any(|w| stderr.contains(w))
}

/// What `ended` means, given what the lines said (`p`). pkexec's 126 and
/// 127 are pkexec's only before the helper's first line: the helper never
/// exits with either.
pub(crate) fn ending(ended: &Ended, p: &Progress) -> Ending {
    let Some(code) = ended.code else {
        return Ending::Failed(match ended.signal {
            Some(sig) => format!("the setup was ended by signal {sig}"),
            None => match &ended.error {
                Some(e) => e.clone(),
                None => "the setup ended without an exit code".into(),
            },
        });
    };
    match Exit::from_code(code) {
        Some(Exit::Done) if p.nothing_to_do => Ending::NothingToDo,
        Some(Exit::Done) => Ending::Done,
        Some(Exit::PlanChanged) => match &p.plan_changed {
            Some(pc) => Ending::PlanChanged(pc.clone()),
            None => {
                Ending::Failed("the helper ended with exit 2 without its plan_changed line".into())
            }
        },
        Some(Exit::StepFailed) => Ending::StepFailed,
        Some(Exit::Refused) => Ending::Refused(
            p.refused
                .clone()
                .unwrap_or_else(|| "the helper ended with exit 4 without its reason".into()),
        ),
        Some(Exit::AlreadyRunning) => {
            Ending::AlreadyRunning(p.already_running.clone().unwrap_or_else(|| "active".into()))
        }
        Some(Exit::TaskFailed) => Ending::TaskFailed(
            p.task_failed
                .clone()
                .unwrap_or_else(|| "the helper ended with exit 6 without its reason".into()),
        ),
        None if !p.authorized && code == 126 => Ending::Dismissed,
        None if !p.authorized && code == 127 && no_agent(&ended.stderr) => Ending::NoAgent,
        None if !p.authorized && code == 127 && not_run(&ended.stderr) => Ending::NotRun,
        None if !p.authorized && code == 127 => Ending::NotAuthorized,
        None => Ending::Failed(format!("the setup ended with exit code {code}")),
    }
}

/// `ended` for the detail: the exit and what was written on stderr.
pub(crate) fn ended_detail(ended: &Ended) -> Vec<String> {
    let mut out = vec![match (ended.code, ended.signal) {
        (Some(code), _) => format!("pkexec and the helper ended with exit code {code}"),
        (None, Some(sig)) => format!("pkexec and the helper were ended by signal {sig}"),
        (None, None) => "pkexec and the helper ended without an exit code".into(),
    }];
    if let Some(e) = &ended.error {
        out.push(e.clone());
    }
    out.extend(
        ended
            .stderr
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .map(|l| format!("stderr: {l}")),
    );
    out
}

/// lmgw's plan in full, for the detail of an exit 2 that is not a change
/// (D88).
pub(crate) fn our_plan(plan: &MachinePlan) -> Vec<String> {
    let mut out = vec![format!("lmgw's plan (sha256 {}):", plan.hash)];
    for s in &plan.steps {
        // The API's name, which is the helper's (`StepKind::as_str`).
        let kind = serde_json::to_value(s.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        out.push(format!("  {kind}"));
        for argv in &s.commands {
            out.push(format!("    {}", command_line(argv)));
        }
        if let Some(f) = &s.writes_file {
            let verb = if f.replaces { "replaces" } else { "creates" };
            out.push(format!("    {verb} {} (mode {:04o}):", f.path, f.mode));
            out.extend(f.content.lines().map(|l| format!("      {l}")));
        }
    }
    out
}

/// Root's plans in full, as the helper's `plan_changed` line carries them
/// (D88).
pub(crate) fn root_plans(plans: &[PlanVariant]) -> Vec<String> {
    let mut out = Vec::new();
    for v in plans {
        let which = if v.update_toolkit {
            "with the toolkit's update"
        } else {
            "without the toolkit's update"
        };
        out.push(format!(
            "the system's plan {which} (sha256 {}, plan schema {}):",
            v.sha256, v.plan.schema
        ));
        if v.plan.steps.is_empty() {
            out.push("  no steps".into());
        }
        for s in &v.plan.steps {
            out.push(format!("  {}", s.kind));
            for argv in &s.commands {
                out.push(format!("    {}", command_line(argv)));
            }
            if let Some(f) = &s.file {
                let verb = if f.replace { "replaces" } else { "creates" };
                out.push(format!("    {verb} {} (mode {:04o}):", f.path, f.mode));
                out.extend(f.content.lines().map(|l| format!("      {l}")));
            }
        }
        out.extend(v.blocked.iter().map(|b| format!("  blocked: {b}")));
    }
    out
}
