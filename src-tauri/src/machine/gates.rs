//! Who may start the setup (machine-check design §3.4): the gates that do
//! not need the machine check, in the order they are asked, each refusing
//! with a plain sentence and its technical reason.
//!
//! The origin gate is not here: the invoke handler in `main` answers every
//! shell command only for the gateway's own document in the app's window
//! (`media::command_allowed`), before the command runs. The gates that need
//! the check (the plan's hash, the button rather than instructions, which
//! covers dnf5) and the one that needs systemd (a setup already running)
//! are the command's own, in [`super::apply`].

use std::path::{Path, PathBuf};

/// The installed program.
pub(crate) const INSTALLED_EXE: &str = "/usr/bin/lmgw";

/// What the gates read about this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Host {
    /// Linux: the only system the helper exists for.
    pub linux: bool,
    /// A release build (`!cfg!(debug_assertions)`, the switch `dev_guard`
    /// uses for the updater).
    pub release: bool,
    /// A dev instance (`state.dev()`: `LMGW_DEV`, the headless runner).
    pub dev: bool,
    /// The running program (`std::env::current_exe`).
    pub exe: Result<PathBuf, String>,
    /// The data dir is the installed app's default.
    pub installed_data_dir: bool,
}

impl Host {
    /// This process.
    pub(crate) fn this(state: &lmgw_core::state::AppState) -> Self {
        Self {
            linux: cfg!(target_os = "linux"),
            release: !cfg!(debug_assertions),
            dev: state.dev(),
            exe: std::env::current_exe().map_err(|e| e.to_string()),
            installed_data_dir: lmgw_core::state::is_installed_data_dir(
                &state.data_dir,
                &lmgw_core::state::default_data_dir(),
                lmgw_core::state::home_default_data_dir().as_deref(),
            ),
        }
    }
}

/// A gate's refusal: what the person reads, and what the fold says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub sentence: String,
    pub detail: Vec<String>,
}

fn refusal(sentence: &str, detail: impl Into<String>) -> Option<Refusal> {
    Some(Refusal {
        sentence: sentence.into(),
        detail: vec![detail.into()],
    })
}

/// Start the installed lmgw: what every "not this copy" refusal tells the
/// person to do.
const START_INSTALLED: &str = "Start lmgw from your app menu, then try again.";

/// The first gate `host` and the helper's installation (`helper`,
/// `lmgw_machine::plan::instructions::helper_installed`, which also checks
/// `/usr/bin/pkexec` at that path, never through `PATH`) refuse, if any.
pub(crate) fn refuse(host: &Host, helper: Result<(), String>) -> Option<Refusal> {
    if !host.linux {
        return refusal(
            "lmgw can set up the graphics card by itself only on Linux.",
            "the setup helper and its polkit action exist for Linux only",
        );
    }
    if !host.release {
        return refusal(
            "This is a development build of lmgw, which never changes the system. Use the \
             installed lmgw, or type the commands under \"Show the commands\" in a terminal.",
            "a debug build never runs the root helper (machine-check design D4)",
        );
    }
    if host.dev {
        return refusal(
            "This lmgw runs as a development copy, which never changes the system. Use the \
             installed lmgw, or type the commands under \"Show the commands\" in a terminal.",
            "a dev instance (LMGW_DEV or the headless runner) never runs the root helper (D4)",
        );
    }
    match &host.exe {
        Err(e) => {
            return refusal(
                &format!("lmgw couldn't tell which program it is, so it can't change the system. {START_INSTALLED}"),
                format!("the running program could not be read: {e}"),
            )
        }
        Ok(exe) if replaced(exe) => {
            return refusal(
                "lmgw was updated while it was running. Restart lmgw, then try again.",
                format!(
                    "the running program is {} (the file was replaced since lmgw started)",
                    exe.display()
                ),
            )
        }
        Ok(exe) if exe != Path::new(INSTALLED_EXE) => {
            return refusal(
                &format!("This copy of lmgw isn't the installed one, so it can't change the system. {START_INSTALLED}"),
                format!("the running program is {}, not {INSTALLED_EXE}", exe.display()),
            )
        }
        Ok(_) => {}
    }
    if !host.installed_data_dir {
        return refusal(
            &format!(
                "This lmgw keeps its data in another folder than the installed app's, so it \
                 can't change the system. {START_INSTALLED}"
            ),
            "the data dir is not the installed app's default (LMGW_DATA_DIR points elsewhere)",
        );
    }
    if let Err(why) = helper {
        return refusal(
            "This lmgw has no working setup helper, so it can't set this up by itself. Type the \
             commands under \"Show the commands\" in a terminal instead.",
            why,
        );
    }
    None
}

/// `/proc/self/exe` names the installed program as replaced: an RPM update
/// installed a new `/usr/bin/lmgw` while this one ran.
fn replaced(exe: &Path) -> bool {
    exe.to_str()
        .is_some_and(|s| s == format!("{INSTALLED_EXE} (deleted)"))
}

/// Asked again after the helper's end where an update while the dialog was
/// open would explain it (pkexec could not run the helper, D142; an exit 2,
/// D143): the detail line when `host`'s program was replaced since lmgw
/// started.
pub(crate) fn updated(host: &Host) -> Option<String> {
    match &host.exe {
        Ok(exe) if replaced(exe) => Some(format!(
            "the running program is {} (an update replaced it since lmgw started)",
            exe.display()
        )),
        _ => None,
    }
}
