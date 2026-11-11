//! §1.1 in the server image: the card passed in or not, and the host's
//! driver: what the host is to do, a restart of the machine only when its
//! libraries are newer than the module it runs.

use lmgw_api_types::machine::{ItemState, MachineCause, MachineItem, MachineItemId};
use lmgw_api_types::RESTART_COMMAND;
use lmgw_machine::driver::DriverState;
use lmgw_machine::pci::Vendor;

use super::super::card::{self, Card};
use super::super::gather::CardFacts;
use super::super::words::item;
use super::{refresh_spec, restart_host, NVIDIACTL};

/// §1.1's item in the server image, and what it concluded.
pub(in crate::machine) fn card(c: &CardFacts) -> (MachineItem, Card) {
    let id = MachineItemId::GraphicsCard;
    if c.has(Vendor::Nvidia) {
        if let DriverState::NoModule { .. } = &c.driver {
            let mut detail = card::base_detail(c);
            detail.push(format!(
                "no NVIDIA kernel module running on the host ({} is not there)",
                lmgw_machine::driver::PROC_NVIDIA_VERSION
            ));
            detail.push(format!(
                "On the host: install NVIDIA's driver (on Fedora RPM Fusion's akmod-nvidia, \
                 https://rpmfusion.org/Howto/NVIDIA), or restart the host so the installed one \
                 loads; once nvidia-smi answers there, refresh the CDI spec (nvidia-ctk cdi \
                 generate --output=/etc/cdi/nvidia.yaml) and restart the service \
                 ({RESTART_COMMAND})"
            ));
            return (
                item(
                    id,
                    ItemState::Problem,
                    "On the host: NVIDIA's driver is not running. Install or start it there, \
                     then restart the lmgw service.",
                    detail,
                    Some(MachineCause::DriverNotRunning { secure_boot: false }),
                ),
                Card::Problem,
            );
        }
        if !c.nvidia_nodes {
            let mut detail = card::base_detail(c);
            if let Some(v) = module_version(&c.driver) {
                detail.push(format!("kernel module {v} runs on the host"));
            }
            detail.push(format!("no {NVIDIACTL} in this container"));
            detail.push(format!(
                "On the host: add AddDevice=nvidia.com/gpu=all to the unit's [Container] \
                 section, check that the host's CDI spec is current (nvidia-ctk cdi list names \
                 nvidia.com/gpu=all), then restart the service ({RESTART_COMMAND})"
            ));
            return (
                item(
                    id,
                    ItemState::Problem,
                    "The lmgw container was started without the graphics card. On the host: add \
                     the card to the unit, check that the host's CDI spec is current, restart \
                     the service.",
                    detail,
                    Some(MachineCause::CardNotPassedIn),
                ),
                Card::Problem,
            );
        }
    } else if c.has(Vendor::Amd) && amd_bound(c) && c.render_nodes.is_empty() {
        let mut detail = card::base_detail(c);
        detail.push(format!(
            "no render node in {} in this container",
            super::super::gather::DRI
        ));
        detail.push(format!(
            "On the host: add AddDevice=/dev/dri (and AddDevice=/dev/kfd for ROCm engines) to \
             the unit's [Container] section, then restart the service ({RESTART_COMMAND})"
        ));
        return (
            item(
                id,
                ItemState::Problem,
                "The lmgw container was started without the graphics card. On the host: add the \
                 card to the unit, then restart the service.",
                detail,
                Some(MachineCause::AmdNoComputeNode),
            ),
            Card::Problem,
        );
    }
    let (mut it, judged) = card::judge(c);
    let card = reword(&mut it, judged);
    (it, card)
}

/// The desktop's §1.1 item for a card that was passed in, reworded for the
/// host (§7) where the desktop's words are this computer's: what it
/// concludes then.
fn reword(it: &mut MachineItem, judged: Card) -> Card {
    match it.cause.clone() {
        // The container keeps the libraries the host's spec mounted at its
        // start; the module is the host's (§7, "after a host driver
        // update"). Libraries newer than the module are a host that has not
        // restarted since its driver update; a module newer than the
        // libraries is a spec the container started with before the update.
        Some(MachineCause::DriverMismatch { kernel, userspace }) => {
            match newer(&userspace, &kernel) {
                Some(true) => {
                    it.sentence = format!(
                        "On the host: restart the machine to load the new driver (module \
                         {kernel}, libraries {userspace})."
                    );
                    it.detail.push(
                        "the host's driver libraries are newer than the module it runs: the new \
                         module loads when the host starts again"
                            .into(),
                    );
                    it.detail.push(restart_host());
                    Card::NeedsRestart
                }
                Some(false) => {
                    it.sentence = format!(
                        "On the host: the driver changed (module {kernel}, libraries \
                         {userspace}). Refresh the host's CDI spec, then restart the lmgw \
                         service."
                    );
                    it.detail.push(
                        "the lmgw container keeps the driver libraries it started with until it \
                         restarts"
                            .into(),
                    );
                    it.detail.push(refresh_spec());
                    Card::Problem
                }
                None => changed_since_start(it),
            }
        }
        // NVML's own mismatch, with no versions to tell which half is new.
        Some(MachineCause::NvmlFailed { .. }) if judged == Card::NeedsRestart => {
            changed_since_start(it)
        }
        Some(MachineCause::NvmlFailed { .. }) => {
            it.sentence = "lmgw couldn't talk to the graphics card through the host's driver. On \
                           the host: check that nvidia-smi answers and that the CDI spec is \
                           current, then restart the lmgw service."
                .into();
            it.detail.push(NVIDIA_SMI.into());
            it.detail.push(refresh_spec());
            judged
        }
        Some(MachineCause::DriverUnreadable { .. }) => {
            it.sentence = "lmgw couldn't read which graphics driver the host runs. On the host: \
                           check that nvidia-smi answers, then restart the lmgw service."
                .into();
            it.detail.push(NVIDIA_SMI.into());
            it.detail.push(refresh_spec());
            judged
        }
        Some(MachineCause::DriverLibrariesMissing) => {
            it.sentence = "The lmgw container got the graphics card without its driver \
                           libraries. On the host: refresh the host's CDI spec, then restart the \
                           lmgw service."
                .into();
            // The desktop's line installs a package on this system; the
            // libraries come from the host's spec here.
            it.detail.retain(|l| !l.contains("sudo dnf install"));
            it.detail.push(refresh_spec());
            Card::Problem
        }
        _ => judged,
    }
}

/// The step that tells whether the host's driver answers at all.
const NVIDIA_SMI: &str = "On the host: run nvidia-smi; it names the card and the driver when \
                          the driver answers there";

/// A change of the host's driver that the versions cannot place: both
/// steps, the machine's restart for an update the host has not restarted
/// for, and the spec's refresh otherwise.
fn changed_since_start(it: &mut MachineItem) -> Card {
    it.sentence = "On the host: the driver changed since the lmgw container started. If the \
                   host has not restarted since its driver update, restart the machine; \
                   otherwise refresh the host's CDI spec, then restart the lmgw service."
        .into();
    it.detail.push(restart_host());
    it.detail.push(refresh_spec());
    Card::Problem
}

/// Whether driver version `a` is newer than `b`, compared part by part as
/// numbers (`600.20.03` against `600.10.01`); `None` when they are the
/// same or either is not dotted numbers.
fn newer(a: &str, b: &str) -> Option<bool> {
    let parts = |v: &str| {
        v.split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    };
    match parts(a)?.cmp(&parts(b)?) {
        std::cmp::Ordering::Greater => Some(true),
        std::cmp::Ordering::Less => Some(false),
        std::cmp::Ordering::Equal => None,
    }
}

/// The running module's version, when the driver state names one.
fn module_version(d: &DriverState) -> Option<&str> {
    match d {
        DriverState::Agree { kernel, .. }
        | DriverState::Differ { kernel, .. }
        | DriverState::UserspaceUnreadable { kernel, .. }
        | DriverState::NoLibraries { kernel } => Some(kernel.version.as_str()),
        _ => None,
    }
}

/// An AMD card on the bus bound to amdgpu (on the host).
fn amd_bound(c: &CardFacts) -> bool {
    c.pci
        .iter()
        .any(|d| d.vendor() == Vendor::Amd && d.driver.as_deref() == Some("amdgpu"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_versions_compare_as_numbers() {
        assert_eq!(newer("600.20.03", "600.10.01"), Some(true));
        assert_eq!(newer("600.10", "600.20"), Some(false));
        assert_eq!(newer("600.40", "600.20"), Some(true));
        assert_eq!(newer("600.20", "600.20"), None);
        assert_eq!(newer("600.20", "unknown"), None);
    }
}
