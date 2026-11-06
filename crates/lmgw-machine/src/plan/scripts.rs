//! What a step's package changes by itself (machine-check design §4.5): the
//! install scripts NVIDIA's package runs as root during the step's dnf
//! transaction, beyond the step's own commands. The confirmation's fold and
//! the plan block show it, so "lmgw will make these changes" is not read as
//! everything that changes. Display only: nothing here is in
//! [`super::HashedPlan`], so no hash moves with this wording.
//!
//! Read from `nvidia-container-toolkit-base` 1.20.1's scripts (`rpm -q
//! --scripts`, 2026-11-03): its `%post` runs `nvidia-ctk config
//! --config-file=/etc/nvidia-container-runtime/config.toml --in-place`
//! (whose comment says an existing file is left as it is) and, on a first
//! install only, `systemctl preset nvidia-cdi-refresh.path
//! nvidia-cdi-refresh.service`, which the package's own
//! `90-nvidia-container-toolkit.preset` turns into enabling both; its
//! `%posttrans`, on every install and update, reloads udev's rules
//! (`udevadm control --reload-rules`), runs `systemctl daemon-reload` and
//! starts both units, whose service creates missing NVIDIA control device
//! nodes and writes `/var/run/cdi/nvidia.yaml` (`nvidia-ctk cdi generate`).
//! A later version can run other scripts; the text is this version's.

use super::steps::{Installer, Step};

/// S4 through dnf and S4″: NVIDIA's package is installed for the first time.
pub const INSTALL_SCRIPTS: &str = "NVIDIA's package then runs its own install scripts: they \
     write /etc/nvidia-container-runtime/config.toml when it is missing, switch on and start \
     NVIDIA's refresh service (nvidia-cdi-refresh), which writes NVIDIA's own description of the \
     card to /var/run/cdi/nvidia.yaml, and reload the system's device rules.";

/// S4′: NVIDIA's packages are updated; the switch-on is a first install's.
pub const UPDATE_SCRIPTS: &str = "NVIDIA's packages then run their own install scripts: they \
     write /etc/nvidia-container-runtime/config.toml when it is missing, start NVIDIA's refresh \
     service (nvidia-cdi-refresh), which writes /var/run/cdi/nvidia.yaml again, and reload the \
     system's device rules.";

/// What `step`'s package changes by itself, if it installs one whose
/// scripts lmgw has read. S1, S1′ and S2 install nothing. S4 through
/// rpm-ostree has no note yet: how rpm-ostree runs these scripts when it
/// puts the new system image together was not read.
pub fn package_scripts(step: &Step) -> Option<&'static str> {
    match step {
        Step::Install {
            via: Installer::Dnf(_),
            ..
        }
        | Step::Swap { .. } => Some(INSTALL_SCRIPTS),
        Step::Update { .. } => Some(UPDATE_SCRIPTS),
        Step::Install {
            via: Installer::RpmOstree,
            ..
        }
        | Step::AddRepo { .. }
        | Step::EnableRepo { .. }
        | Step::AddKey { .. } => None,
    }
}
