//! §1.2, using the card for models (NVIDIA): who owns `nvidia-ctk` (D24),
//! whether lmgw's own spec could be written, the system's specs for
//! information, NVIDIA's software source, and the plan (§3.2) as the API
//! shows it.

use lmgw_api_types::machine::{
    ItemState, MachineCause, MachineItem, MachineItemId, MachinePlan, PlanFile, PlanHow, PlanStep,
    PlanStepKind,
};
use lmgw_machine::cdi::own::{Judgement, NVIDIA_CTK};
use lmgw_machine::plan::embedded::{
    NVIDIA_BASEURL_PREFIX, NVIDIA_KEY_ID, REPO_ID, TOOLKIT_BASE, TOOLKIT_FULL,
};
use lmgw_machine::plan::{
    self, how, terminal_lines, HashedPlan, How, Plan, Step, StepKind, Toolkit, UserSide, Want,
    PROVIDERS,
};

use super::gather::{RepoQuery, ToolkitFacts};
use super::words::{day, item, utc};
use crate::hf::fmt_bytes;
use crate::runtime::cdi::{CdiError, DefaultDirs, Ensured};

/// What §1.2 concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Toolkit12 {
    Ok,
    /// A problem the plan fixes behind the button.
    Fixable,
    /// A problem fixed by instructions.
    Steps,
    /// A problem a restart of the computer fixes.
    Restart,
}

/// Why lmgw's own spec was not written the last time, sorted by who is to
/// blame (§1.2, D24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WriteFailed {
    /// The toolkit that writes it failed, or wrote a spec naming paths that
    /// are not there: its update (S4′) is offered.
    Toolkit(String),
    /// The write ran out of the load timeout: the card's driver stopped
    /// answering (NVML hung, a card off the bus).
    Card(String),
    /// A folder lmgw keeps it in is not private.
    NotPrivate(String),
    /// lmgw's own files could not be written or read.
    Files(String),
}

/// What the last write's error says about who is to blame; `None` when it
/// is nobody's to fix: a write that is blocked (§1.1 owns the driver), the
/// machine that changed mid-write, or another write holding the lock (the
/// next start or check decides).
pub(super) fn write_failed(e: &CdiError) -> Option<WriteFailed> {
    match e {
        CdiError::Generate { .. } | CdiError::Incomplete(_) => {
            Some(WriteFailed::Toolkit(e.to_string()))
        }
        CdiError::TimedOut { .. } => Some(WriteFailed::Card(e.to_string())),
        CdiError::NotPrivate { .. } => Some(WriteFailed::NotPrivate(e.to_string())),
        CdiError::Io(_) => Some(WriteFailed::Files(e.to_string())),
        CdiError::Blocked(_) | CdiError::Changed(_) | CdiError::Waited { .. } => None,
    }
}

/// The plan's step kind as the API names it.
fn kind(k: StepKind) -> PlanStepKind {
    match k {
        StepKind::AddRepo => PlanStepKind::S1AddRepo,
        StepKind::EnableRepo => PlanStepKind::S1EnableRepo,
        StepKind::AddKey => PlanStepKind::S2AddKey,
        StepKind::Install => PlanStepKind::S4Install,
        StepKind::Update => PlanStepKind::S4Update,
        StepKind::Swap => PlanStepKind::S4Swap,
    }
}

/// The download a toolkit step fetches, when the user's metadata cache
/// says: the planned package's row from the planned section.
fn size_of(step: &Step, section: Option<&str>, q: &RepoQuery) -> Option<u64> {
    let package = match step {
        Step::Install { package, .. } => package.as_str(),
        Step::Update { packages, .. } => packages.first.as_str(),
        Step::Swap { to, .. } => to.as_str(),
        _ => return None,
    };
    q.rows
        .iter()
        .find(|(name, _, repo, _)| name == package && Some(repo.as_str()) == section)
        .and_then(|r| r.3)
}

/// `p` as the API shows it, carried out `h`. The hash is always over the
/// steps it shows.
pub(super) fn plan_dto(p: &Plan, h: &How, t: &ToolkitFacts) -> MachinePlan {
    let section = p.section.as_ref().map(|s| s.id.as_str());
    MachinePlan {
        hash: p.hash(),
        how: match h {
            How::Helper => PlanHow::Helper,
            How::Instructions { why } => PlanHow::Instructions { why: why.clone() },
        },
        steps: p
            .steps
            .iter()
            .map(|s| {
                let bytes = size_of(s, section, &t.repoquery);
                let mut sentence = match (s, t.inputs.toolkit.package()) {
                    (Step::Swap { from, .. }, Some(pkg)) => format!(
                        "Replace the piece Fedora stopped updating ({} {}) with NVIDIA's current one",
                        from.as_str(),
                        pkg.version
                    ),
                    _ => s.sentence(),
                };
                if let Some(b) = bytes {
                    sentence.push_str(&format!(" · {} download", fmt_bytes(b)));
                }
                PlanStep {
                    kind: kind(s.kind()),
                    sentence,
                    commands: s.commands(),
                    writes_file: s.file().map(|w| PlanFile {
                        path: w.path.to_string(),
                        content: w.content.to_string(),
                        mode: w.mode,
                        replaces: w.replace,
                        key_fingerprint: super::key::fingerprint(w.content),
                    }),
                    download_bytes: bytes,
                    terminal: terminal_lines(s),
                    package_scripts: plan::package_scripts(s).map(str::to_string),
                }
            })
            .collect(),
    }
}

/// An enabled repository outside the provider table and NVIDIA's own that
/// offers the toolkit (§3.2), and the package to install from it (`-base`
/// when it offers that): the plan installs from there alone, as
/// instructions, rather than adding NVIDIA's source next to it.
fn other_provider(t: &ToolkitFacts) -> Option<(String, String)> {
    let offers = |name: &str| {
        t.repoquery
            .rows
            .iter()
            .filter(move |(n, ..)| n == name)
            .map(|(n, _, repo, _)| (repo.clone(), n.clone()))
            .find(|(repo, _)| {
                let nvidia = t.inputs.repos.get(repo).is_some_and(|s| {
                    s.baseurl
                        .iter()
                        .any(|u| u.starts_with(NVIDIA_BASEURL_PREFIX))
                });
                let table = PROVIDERS.iter().any(|p| p.repo_id == repo.as_str());
                !nvidia && !table && repo.as_str() != REPO_ID
            })
    };
    offers(TOOLKIT_BASE).or_else(|| offers(TOOLKIT_FULL))
}

/// Why lmgw's own spec could not be written the last time, when that is
/// someone's to fix ([`write_failed`]).
fn generation_failed(t: &ToolkitFacts) -> Option<WriteFailed> {
    match &t.spec.last.as_ref()?.outcome {
        Ok(_) => None,
        Err(e) => write_failed(e),
    }
}

/// §1.2's item, the plan it offers, and what it concluded.
pub(super) fn judge(t: &ToolkitFacts) -> (MachineItem, Option<MachinePlan>, Toolkit12) {
    let id = MachineItemId::CardForModels;
    let mut detail = Vec::new();
    let owner = t.inputs.toolkit.owner_text();
    let version = match (&t.ctk_version, &t.inputs.toolkit) {
        (Ok(v), _) => format!(" · nvidia-ctk {v}"),
        (Err(_), Toolkit::Absent) => String::new(),
        (Err(e), _) => format!(" · {e}"),
    };
    detail.push(format!(
        "NVIDIA Container Toolkit: {NVIDIA_CTK} owned by {owner}{version}"
    ));
    spec_detail(t, &mut detail);
    match &t.system_all {
        Some(file) => detail.push(format!(
            "your own containers use {}, which lmgw's models don't read",
            file.display()
        )),
        None => detail.push(format!(
            "no spec in the system's spec dirs ({}) defines nvidia.com/gpu=all",
            t.system_dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
    for s in &t.inputs.repos.skipped {
        detail.push(format!("{} not read: {}", s.path.display(), s.why));
    }
    if let Some(why) = &t.repoquery.skipped {
        detail.push(format!("no download sizes: {why}"));
    }
    if let Err(why) = &t.helper {
        detail.push(format!("no button for the setup: {why}"));
    }
    // A write that failed for a reason the toolkit is not to blame for: no
    // plan, the cause of its own.
    let failed = match generation_failed(t) {
        Some(WriteFailed::Card(error)) => {
            detail.push(format!("the last write failed: {error}"));
            detail.push(format!(
                "if the card is only slow to answer, a longer {} gives it more time",
                crate::config::LOAD_TIMEOUT_LABEL
            ));
            return (
                item(
                    id,
                    ItemState::Problem,
                    "Your graphics card stopped answering while lmgw prepared it for models. \
                     Restart the computer.",
                    detail,
                    Some(MachineCause::SpecWriteTimedOut { error }),
                ),
                None,
                Toolkit12::Restart,
            );
        }
        Some(WriteFailed::NotPrivate(error)) => {
            detail.push(format!("the last write failed: {error}"));
            return (
                item(
                    id,
                    ItemState::Problem,
                    "lmgw keeps a file about your graphics card in a folder only you may open, \
                     and that folder is open to others. The details say which folder; make it \
                     yours alone, or remove it so lmgw makes it again.",
                    detail,
                    Some(MachineCause::SpecDirNotPrivate { error }),
                ),
                None,
                Toolkit12::Steps,
            );
        }
        Some(WriteFailed::Files(error)) => {
            detail.push(format!("the last write failed: {error}"));
            return (
                item(
                    id,
                    ItemState::Problem,
                    "lmgw couldn't save a file about your graphics card. The details say why.",
                    detail,
                    Some(MachineCause::SpecFilesFailed { error }),
                ),
                None,
                Toolkit12::Steps,
            );
        }
        Some(WriteFailed::Toolkit(error)) => Some(error),
        None => None,
    };
    // Another repository offering the toolkit matters only to its install.
    let other = matches!(t.inputs.toolkit, Toolkit::Absent)
        .then(|| other_provider(t))
        .flatten();
    if let Some((repo, package)) = &other {
        detail.push(format!(
            "the software source {repo} offers the toolkit ({package}), so the install comes \
             from there"
        ));
    }
    let user = UserSide {
        appimage: t.appimage,
        helper: t.helper.clone(),
        other_provider: other.as_ref().map(|(repo, _)| repo.clone()),
        read_differently: t.read_differently.clone(),
    };
    let planned = |want: Want| {
        let p = plan::plan(&t.inputs, want, PROVIDERS);
        let h = how(&p, &user);
        (plan_dto(&p, &h, t), h)
    };
    match (&t.inputs.toolkit, failed) {
        (Toolkit::Absent, _) => {
            let (p, cause) = match &other {
                Some((repo, package)) => (
                    plan::plan_from_other(&t.inputs, repo, package),
                    MachineCause::ToolkitOtherProvider { repo: repo.clone() },
                ),
                None => (
                    plan::plan(&t.inputs, Want::default(), PROVIDERS),
                    MachineCause::ToolkitMissing,
                ),
            };
            let h = how(&p, &user);
            let dto = plan_dto(&p, &h, t);
            let (sentence, outcome) = match (&h, dto.steps.is_empty()) {
                (How::Helper, _) => (
                    "One piece is missing so models can use the graphics card. lmgw can set it up.",
                    Toolkit12::Fixable,
                ),
                (How::Instructions { .. }, false) => (
                    "One piece is missing so models can use the graphics card. Here's how to add it.",
                    Toolkit12::Steps,
                ),
                (How::Instructions { .. }, true) => (
                    "One piece is missing so models can use the graphics card. lmgw can't add it \
                     here; the details say why.",
                    Toolkit12::Steps,
                ),
            };
            detail.push(format!(
                "NVIDIA Container Toolkit not installed ({NVIDIA_CTK} is not there)"
            ));
            (
                item(id, ItemState::Problem, sentence, detail, Some(cause)),
                Some(dto),
                outcome,
            )
        }
        (Toolkit::Nvidia(_), Some(error)) => {
            detail.push(format!("the last write failed: {error}"));
            let (dto, h) = planned(Want {
                update_toolkit: true,
            });
            let (sentence, outcome) = match (&h, dto.steps.is_empty()) {
                (How::Helper, _) => (
                    "lmgw couldn't write the description of your graphics card that models use. \
                     lmgw can update the piece that writes it.",
                    Toolkit12::Fixable,
                ),
                (How::Instructions { .. }, false) => (
                    "lmgw couldn't write the description of your graphics card that models use. \
                     Here's how to update the piece that writes it.",
                    Toolkit12::Steps,
                ),
                (How::Instructions { .. }, true) => (
                    "lmgw couldn't write the description of your graphics card that models use. \
                     Updating the piece that writes it should fix this; the details say how.",
                    Toolkit12::Steps,
                ),
            };
            (
                item(
                    id,
                    ItemState::Problem,
                    sentence,
                    detail,
                    Some(MachineCause::SpecNotWritten { error, owner }),
                ),
                Some(dto),
                outcome,
            )
        }
        (Toolkit::Retired(pkg), failed) => {
            let (dto, h) = planned(Want::default());
            let helper = matches!(h, How::Helper);
            let shown = !dto.steps.is_empty();
            let cause = Some(MachineCause::ToolkitRetired {
                package: pkg.name.clone(),
                version: pkg.version.clone(),
            });
            detail.push(format!(
                "{} is retired: Fedora no longer updates it (Fedora bug 2434398)",
                pkg.name
            ));
            match failed {
                None => (
                    item(
                        id,
                        ItemState::Info,
                        match (helper, shown) {
                            (true, _) => {
                                "Models can use the graphics card. One piece comes from a package \
                                 Fedora no longer updates; lmgw can switch it to NVIDIA's."
                            }
                            (false, true) => {
                                "Models can use the graphics card. One piece comes from a package \
                                 Fedora no longer updates; here's how to switch it to NVIDIA's."
                            }
                            (false, false) => {
                                "Models can use the graphics card. One piece comes from a package \
                                 Fedora no longer updates; the details say how to switch it."
                            }
                        },
                        detail,
                        cause,
                    ),
                    Some(dto),
                    Toolkit12::Ok,
                ),
                Some(error) => {
                    detail.push(format!("the last write failed: {error}"));
                    (
                        item(
                            id,
                            ItemState::Problem,
                            match (helper, shown) {
                                (true, _) => {
                                    "lmgw couldn't write the description of your graphics card \
                                     that models use. lmgw can replace the piece that writes it."
                                }
                                (false, true) => {
                                    "lmgw couldn't write the description of your graphics card \
                                     that models use. Here's how to replace the piece that writes \
                                     it."
                                }
                                (false, false) => {
                                    "lmgw couldn't write the description of your graphics card \
                                     that models use. Replacing the piece that writes it should \
                                     fix this; the details say how."
                                }
                            },
                            detail,
                            cause,
                        ),
                        Some(dto),
                        if helper {
                            Toolkit12::Fixable
                        } else {
                            Toolkit12::Steps
                        },
                    )
                }
            }
        }
        (_, Some(error)) => {
            // An owner lmgw does not manage: no steps, and the why says what
            // to do instead.
            detail.push(format!("the last write failed: {error}"));
            let why = format!(
                "lmgw doesn't manage this piece ({owner}), so it leaves it as it is. Updating or \
                 reinstalling it from where it came from usually fixes this."
            );
            let dto = MachinePlan {
                hash: HashedPlan::of(&[]).sha256(),
                how: PlanHow::Instructions { why },
                steps: Vec::new(),
            };
            (
                item(
                    id,
                    ItemState::Problem,
                    "lmgw couldn't write the description of your graphics card that models use. \
                     Updating the piece that writes it from where it came from usually fixes \
                     this.",
                    detail,
                    Some(MachineCause::SpecNotWritten { error, owner }),
                ),
                Some(dto),
                Toolkit12::Steps,
            )
        }
        (toolkit, None) => {
            if let Toolkit::Nvidia(p) = toolkit {
                if p.sig_key_id.as_deref() == Some(NVIDIA_KEY_ID) {
                    detail.push(format!(
                        "toolkit {} from NVIDIA (signed by key {NVIDIA_KEY_ID})",
                        p.version
                    ));
                }
            }
            // A plan can be due with nothing wrong: lmgw keeping its own
            // repository file or NVIDIA's key current.
            let p = plan::plan(&t.inputs, Want::default(), PROVIDERS);
            let offered = (!p.steps.is_empty()).then(|| plan_dto(&p, &how(&p, &user), t));
            if let Some(f) = &t.frozen {
                detail.push(format!(
                    "NVIDIA's software source ({}) last loaded {} UTC; fedora/updates loaded {} \
                     UTC; it expires after {} h",
                    f.section,
                    utc(f.last_loaded_ns),
                    utc(f.others_loaded_ns),
                    f.expire_secs / 3600
                ));
                return (
                    item(
                        id,
                        ItemState::Info,
                        &format!(
                            "Models can use the graphics card. lmgw's graphics card piece hasn't \
                             been able to get updates since {}.",
                            day(f.last_loaded_ns)
                        ),
                        detail,
                        Some(MachineCause::ToolkitSourceFrozen {
                            section: f.section.clone(),
                            last_loaded: utc(f.last_loaded_ns),
                        }),
                    ),
                    offered,
                    Toolkit12::Ok,
                );
            }
            let cause = t
                .spec
                .default_dirs
                .map(|d| MachineCause::SpecOnSystemDirs { why: d.to_string() });
            (
                item(
                    id,
                    ItemState::Ok,
                    "Models can use the graphics card.",
                    detail,
                    cause,
                ),
                offered,
                Toolkit12::Ok,
            )
        }
    }
}

/// lmgw's own spec in the detail: where, what it was written for, whether it
/// is true of the machine now, the last write, and Podman's default dirs
/// when lmgw writes none (D26).
fn spec_detail(t: &ToolkitFacts, detail: &mut Vec<String>) {
    let s = &t.spec;
    if let Some(why) = s.default_dirs {
        detail.push(match why {
            DefaultDirs::NoToolkit => {
                "lmgw writes no description of its own yet: the toolkit is not installed".into()
            }
            DefaultDirs::PodmanWithoutOption => why.to_string(),
        });
    }
    if let Some(p) = &s.paths {
        detail.push(format!("lmgw's own description: {}", p.spec.display()));
    }
    if let Some(stamp) = &s.stamp {
        detail.push(format!(
            "written {} UTC for driver {} with nvidia-ctk {}",
            stamp.written_at,
            stamp.userspace.as_deref().unwrap_or("unknown"),
            stamp.nvidia_ctk.version.as_deref().unwrap_or("unknown")
        ));
    }
    match &s.now {
        Some(Judgement::Fresh) => detail.push("true of this machine now".into()),
        Some(Judgement::Stale(why)) => detail.push(format!(
            "lmgw writes it again before the next model start: {}",
            why.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )),
        Some(Judgement::Blocked(b)) => detail.push(format!("not written now: {b}")),
        None => {}
    }
    if let Some(last) = &s.last {
        let what = match &last.outcome {
            Ok(Ensured::Fresh) => "it was true of the machine".to_string(),
            Ok(Ensured::Written(why)) => format!(
                "written ({})",
                why.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Ok(Ensured::DefaultDirs(d)) => d.to_string(),
            Ok(Ensured::Unmanaged) => "lmgw manages no description here".into(),
            Err(e) => format!("failed: {e}"),
        };
        detail.push(format!(
            "last looked at {}: {what}",
            last.at.format("%Y-%m-%d %H:%M:%S UTC")
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use lmgw_machine::cdi::own::{Blocked, Stale};

    use super::*;

    /// Every way the last write can fail, and whose it is: only the
    /// toolkit's own failures offer its update; running out of the load
    /// timeout is the card's; a folder that is not private and lmgw's own
    /// files are their own causes; a blocked write, a machine that changed
    /// mid-write and another write holding the lock are nobody's to fix.
    #[test]
    fn each_write_failure_is_blamed_on_what_failed() {
        let generate = CdiError::Generate {
            status: "1".into(),
            said: vec!["failed to initialize NVML".into()],
        };
        assert!(matches!(
            write_failed(&generate),
            Some(WriteFailed::Toolkit(e)) if e.contains("nvidia-ctk cdi generate")
        ));
        let incomplete = CdiError::Incomplete(vec![(
            PathBuf::from("/usr/bin/nvidia-smi"),
            "not there".into(),
        )]);
        assert!(matches!(
            write_failed(&incomplete),
            Some(WriteFailed::Toolkit(_))
        ));
        let timed_out = CdiError::TimedOut {
            program: "nvidia-ctk".into(),
            ran: Duration::from_secs(600),
            limit: Duration::from_secs(600),
        };
        assert!(matches!(
            write_failed(&timed_out),
            Some(WriteFailed::Card(e)) if e.contains("Load timeout")
        ));
        let not_private = CdiError::NotPrivate {
            dir: PathBuf::from("/run/user/1000/lmgw-machine"),
            why: "it is mode 0777".into(),
        };
        assert!(matches!(
            write_failed(&not_private),
            Some(WriteFailed::NotPrivate(e)) if e.contains("/run/user/1000/lmgw-machine")
        ));
        assert!(matches!(
            write_failed(&CdiError::Io("disk full".into())),
            Some(WriteFailed::Files(e)) if e.contains("disk full")
        ));
        for nobodys in [
            CdiError::Waited {
                waited: Duration::from_secs(600),
            },
            CdiError::Changed(vec![Stale::NoSpec]),
            CdiError::Blocked(Blocked::NoModule),
            CdiError::Blocked(Blocked::Restart {
                kernel: "600.10.01".into(),
                userspace: "600.20.03".into(),
            }),
        ] {
            assert_eq!(write_failed(&nobodys), None, "{nobodys:?}");
        }
    }
}
