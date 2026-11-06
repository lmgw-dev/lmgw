//! Other systems (machine-check design §3.7): where the plan runs behind the
//! button and where it is shown as commands to type, and those commands.
//!
//! Only the user's side decides this ([`UserSide`]: an AppImage, whether the
//! helper is installed, another repository offering the toolkit), so none of
//! it is in the hash: root never sees it.
//!
//! A `why` promises commands only when the plan has steps: a plan with none
//! (blocked, a system lmgw has no commands for, an update on a read-only
//! system) says what is in the way and what the person can do instead.

use std::path::Path;

use crate::facts::{Facts, Kind, Meta};

use super::embedded::{HELPER, NVIDIA_INSTALL_GUIDE, PKEXEC, POLICY};
use super::steps::{Installer, Step};
use super::{Plan, System, Toolkit};

/// What the check knows that root does not, and that decides between the
/// button and instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSide {
    /// lmgw runs from an AppImage (`APPIMAGE` is set).
    pub appimage: bool,
    /// The helper and its polkit action are installed and sane; why not.
    pub helper: Result<(), String>,
    /// An enabled repository outside the provider table offers the toolkit
    /// (the advisory `dnf repoquery`) and the plan installs it from there
    /// ([`super::plan_from_other`]): its id.
    pub other_provider: Option<String>,
    /// The hashes of plans the helper answered with exit 2 while the shell,
    /// computing the plan again, still got one root does not compute (D88):
    /// lmgw and the system read this computer differently, so a password
    /// would only be asked for a plan root refuses again.
    pub read_differently: Vec<String>,
}

/// Why a plan the helper read differently (D88) gets instructions.
pub const READ_DIFFERENTLY: &str = "lmgw and your system read this computer differently, so lmgw \
                                    can't set it up by itself. Here are the commands to run in a \
                                    terminal instead.";

/// How the plan is carried out here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// The button: the helper runs the steps after one password.
    Helper,
    /// Commands to type in a terminal, and why there is no button.
    Instructions { why: String },
}

/// How `plan` is carried out on a system whose user side is `user`.
pub fn how(plan: &Plan, user: &UserSide) -> How {
    let instructions = |why: String| How::Instructions { why };
    if !plan.blocked.is_empty() {
        let mut todo: Vec<String> = Vec::new();
        for b in &plan.blocked {
            let t = b.what_to_do();
            if !todo.contains(&t) {
                todo.push(t);
            }
        }
        return instructions(format!(
            "lmgw can't set this up by itself: {}. {}",
            plan.blocked
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
            todo.join(" ")
        ));
    }
    match plan.system {
        System::NoDnf => {
            return instructions(format!(
                "lmgw doesn't know this system's package manager yet. NVIDIA's install guide \
                 says how to add the piece: {NVIDIA_INSTALL_GUIDE}"
            ))
        }
        System::Ostree if plan.steps.is_empty() => {
            return instructions(match plan.toolkit {
                Toolkit::Retired(_) => format!(
                    "This system's software is a read-only image, and lmgw has no commands for \
                     replacing this piece on it yet. NVIDIA's install guide says how: \
                     {NVIDIA_INSTALL_GUIDE}"
                ),
                _ => "This system's software is a read-only image: NVIDIA's piece is updated \
                      with the rest of it. Install your system's updates (or run rpm-ostree \
                      upgrade in a terminal), then restart the computer."
                    .into(),
            })
        }
        System::Ostree => {
            return instructions(
                "This system's software is a read-only image, so lmgw can't change it by \
                 itself. Here are the commands to run in a terminal; the last one restarts the \
                 computer."
                    .into(),
            )
        }
        System::OtherDnf => {
            return instructions(format!(
                "lmgw's commands are written for dnf5, and this system has an older dnf, so lmgw \
                 shows none here. NVIDIA's install guide has the commands for it: \
                 {NVIDIA_INSTALL_GUIDE}"
            ))
        }
        System::Dnf5 if plan.steps.is_empty() => {
            return instructions("Nothing needs to be run on this computer.".into())
        }
        System::Dnf5 => {}
    }
    if user.appimage {
        return instructions(
            "lmgw from an AppImage can't change the system, so here are the commands to run in \
             a terminal. Or install lmgw's RPM, which can do this for you."
                .into(),
        );
    }
    if let Some(repo) = &user.other_provider {
        return instructions(format!(
            "Your computer already has a software source that offers this piece ({repo}), so \
             lmgw doesn't add NVIDIA's next to it. Here's the command to install it from there."
        ));
    }
    if user.helper.is_err() {
        return instructions(
            "This version of lmgw can't do this by itself yet, so here are the commands to run \
             in a terminal."
                .into(),
        );
    }
    if user.read_differently.contains(&plan.hash()) {
        return instructions(READ_DIFFERENTLY.into());
    }
    How::Helper
}

/// `argv` as one line, each argument quoted as a terminal reads it: how the
/// shell's own confirmation shows a command root runs (WP7), the program by
/// its absolute path.
pub fn command_line(argv: &[String]) -> String {
    argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// Shell-quote one argument for a terminal line: bare when it is made of
/// characters no shell reads specially, in single quotes otherwise.
fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:=,+@%-".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// `step` as lines to type in a terminal, with `sudo`: a file it writes as a
/// `sudo tee` here-document (quoted, so nothing in it is expanded), then its
/// commands. An rpm-ostree install ends with the restart it needs.
pub fn terminal_lines(step: &Step) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(w) = step.file() {
        lines.push(format!("sudo tee {} > /dev/null <<'EOF'", quote(w.path)));
        lines.extend(w.content.lines().map(str::to_string));
        lines.push("EOF".into());
    }
    for argv in step.commands() {
        let shown: Vec<String> = argv
            .iter()
            .enumerate()
            .map(|(i, a)| {
                // The program by its plain name: a terminal finds it in PATH.
                if i == 0 {
                    Path::new(a)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| a.clone())
                } else {
                    quote(a)
                }
            })
            .collect();
        lines.push(format!("sudo {}", shown.join(" ")));
    }
    if matches!(
        step,
        Step::Install {
            via: Installer::RpmOstree,
            ..
        }
    ) {
        lines.push("systemctl reboot".into());
    }
    lines
}

fn root_owned_regular(meta: &Meta, path: &str, max_mode: u32) -> Result<(), String> {
    if meta.kind == Kind::Symlink {
        return Err(format!("{path} is a symbolic link"));
    }
    if meta.kind != Kind::File {
        return Err(format!("{path} is not a regular file"));
    }
    if meta.uid != 0 {
        return Err(format!("{path} belongs to user {}, not to root", meta.uid));
    }
    if meta.mode & 0o7000 != 0 {
        return Err(format!("{path} has a setuid, setgid or sticky bit"));
    }
    if meta.mode & !max_mode & 0o777 != 0 {
        return Err(format!(
            "{path} has mode {:04o}, more than {max_mode:04o}",
            meta.mode & 0o7777
        ));
    }
    Ok(())
}

/// Is the helper installed as §3.4 requires: a root-owned regular file of
/// mode 0755 or stricter with no setuid, setgid or sticky bit, in root-owned
/// directories only root can write; its polkit action a root-owned regular
/// file nobody else can write; and `/usr/bin/pkexec` there? The shell checks
/// the same again before it starts anything (WP7); the check uses it to
/// choose between the button and instructions.
pub fn helper_installed(f: &dyn Facts) -> Result<(), String> {
    let lstat = |p: &str| f.lstat(Path::new(p)).map_err(|e| (p.to_string(), e));
    match lstat(HELPER) {
        Err((p, e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "this lmgw has no setup helper installed ({p} is not there)"
            ))
        }
        Err((p, e)) => return Err(format!("{p} could not be read: {e}")),
        Ok(meta) => root_owned_regular(&meta, HELPER, 0o755)?,
    }
    for dir in Path::new(HELPER).ancestors().skip(1) {
        let d = dir.to_string_lossy();
        let meta = f
            .lstat(dir)
            .map_err(|e| format!("{d} could not be read: {e}"))?;
        if meta.kind != Kind::Dir || meta.uid != 0 || meta.mode & 0o022 != 0 {
            return Err(format!("{d} is not a directory that only root can write"));
        }
    }
    match lstat(POLICY) {
        Err((p, e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "the setup's polkit action is not installed ({p} is not there)"
            ))
        }
        Err((p, e)) => return Err(format!("{p} could not be read: {e}")),
        Ok(meta) => root_owned_regular(&meta, POLICY, 0o644)?,
    }
    if f.stat(Path::new(PKEXEC)).is_err() {
        return Err(format!("{PKEXEC} is not there"));
    }
    Ok(())
}
