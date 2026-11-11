//! §1.1, the graphics card and its driver: which cards are on the bus, the
//! NVIDIA driver's two halves (the running kernel module against the
//! libraries on disk), NVML in a separate process once they agree, Secure
//! Boot, and amdgpu's counters.

use lmgw_api_types::machine::{ItemState, MachineCause, MachineItem, MachineItemId};
use lmgw_machine::driver::{DriverState, ModuleFile, ModuleFlavor, Read, SecureBoot};
use lmgw_machine::pci::{PciDevice, Vendor};

use super::gather::{CardFacts, DRI, KFD};
use super::probe::NvmlAnswer;
use super::words::{item, utc};
use crate::hf::fmt_bytes;

/// What §1.1 concluded, for the items after it and the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Card {
    /// An NVIDIA card whose driver works.
    NvidiaOk,
    /// An AMD card lmgw can compute on: amdgpu, with render nodes.
    AmdOk,
    /// No NVIDIA or AMD card.
    None,
    /// NVIDIA's card on another driver.
    NeedsDriver,
    /// A restart finishes the driver's update or start.
    NeedsRestart,
    /// Something else is wrong with the card or its driver.
    Problem,
}

fn device_name(d: &PciDevice) -> String {
    match (&d.vendor_name, &d.device_name) {
        (Some(v), Some(n)) => format!("{v} {n}"),
        (None, Some(n)) => n.clone(),
        _ => format!("{:04x}:{:04x}", d.vendor_id, d.device_id),
    }
}

fn pci_line(d: &PciDevice) -> String {
    format!(
        "PCI {} {:04x}:{:04x} {} · driver {}",
        d.address,
        d.vendor_id,
        d.device_id,
        device_name(d),
        d.driver.as_deref().unwrap_or("none")
    )
}

fn flavor(f: ModuleFlavor) -> &'static str {
    match f {
        ModuleFlavor::Open => "open",
        ModuleFlavor::Proprietary => "proprietary",
    }
}

/// The detail every §1.1 item starts with: the display devices on the bus
/// and the driver packages found.
pub(super) fn base_detail(c: &CardFacts) -> Vec<String> {
    let mut detail: Vec<String> = c.pci.iter().map(pci_line).collect();
    if c.pci.is_empty() {
        detail.push("no display device on the PCI bus".into());
    }
    if !c.driver_packages.is_empty() {
        detail.push(format!(
            "driver packages: {}",
            c.driver_packages
                .iter()
                .map(|p| p.nvra())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    detail
}

/// §1.1's item, and what it concluded.
pub(super) fn judge(c: &CardFacts) -> (MachineItem, Card) {
    let detail = base_detail(c);
    if c.has(Vendor::Nvidia) {
        return nvidia(c, detail);
    }
    if c.has(Vendor::Amd) {
        return amd(c, detail);
    }
    let others: Vec<String> = c.pci.iter().map(device_name).collect();
    (
        item(
            MachineItemId::GraphicsCard,
            ItemState::Problem,
            "No graphics card found. Cloud models work; local models need a graphics card.",
            detail,
            Some(MachineCause::NoCard { others }),
        ),
        Card::None,
    )
}

/// What NVML in a separate process said, once it is the one to decide
/// (§1.1): the driver's halves agree, or the libraries' version could not be
/// read and NVML's own start compares them (`version_mismatch`).
fn by_nvml(c: &CardFacts, mut detail: Vec<String>) -> (MachineItem, Card) {
    let id = MachineItemId::GraphicsCard;
    match &c.nvml {
        Some(Ok(NvmlAnswer::Ok {
            driver_version,
            cards,
            ..
        })) => {
            detail.push(format!(
                "NVML in a separate process: driver {}, {} card(s)",
                driver_version.as_deref().unwrap_or("unknown"),
                cards.len()
            ));
            for card in cards {
                detail.push(format!(
                    "card {}: {}{}{}",
                    card.index,
                    card.name,
                    card.uuid
                        .as_deref()
                        .map(|u| format!(" · {u}"))
                        .unwrap_or_default(),
                    card.pci_bus_id
                        .as_deref()
                        .map(|b| format!(" · PCI {b}"))
                        .unwrap_or_default()
                ));
            }
            let names: Vec<String> = cards
                .iter()
                .map(|k| match k.memory_total {
                    Some(b) => format!("{} · {}", k.name, fmt_bytes(b)),
                    None => k.name.clone(),
                })
                .collect();
            let sentence = if names.is_empty() {
                "Graphics card · NVIDIA".to_string()
            } else if names.len() == 1 {
                format!("Graphics card · {}", names[0])
            } else {
                format!("Graphics cards · {}", names.join(", "))
            };
            (
                item(id, ItemState::Ok, &sentence, detail, None),
                Card::NvidiaOk,
            )
        }
        Some(Ok(NvmlAnswer::Failed { kind, error })) => {
            detail.push(format!(
                "NVML in a separate process failed ({kind}): {error}"
            ));
            let (sentence, card) = if kind == "version_mismatch" {
                (
                    "Restart the computer to finish the graphics driver update.",
                    Card::NeedsRestart,
                )
            } else {
                (
                    "lmgw couldn't talk to the graphics card through its driver.",
                    Card::Problem,
                )
            };
            (
                item(
                    id,
                    ItemState::Problem,
                    sentence,
                    detail,
                    Some(MachineCause::NvmlFailed {
                        error: format!("{kind}: {error}"),
                    }),
                ),
                card,
            )
        }
        Some(Err(e)) => {
            detail.push(format!("the NVML probe could not run: {e}"));
            (
                item(
                    id,
                    ItemState::Problem,
                    "lmgw couldn't talk to the graphics card through its driver.",
                    detail,
                    Some(MachineCause::NvmlFailed { error: e.clone() }),
                ),
                Card::Problem,
            )
        }
        None => {
            detail.push("NVML not asked".into());
            (
                item(
                    id,
                    ItemState::Problem,
                    "lmgw couldn't talk to the graphics card through its driver.",
                    detail,
                    Some(MachineCause::NvmlFailed {
                        error: "not asked".into(),
                    }),
                ),
                Card::Problem,
            )
        }
    }
}

fn nvidia(c: &CardFacts, mut detail: Vec<String>) -> (MachineItem, Card) {
    let id = MachineItemId::GraphicsCard;
    match &c.driver {
        DriverState::Agree { kernel, userspace } => {
            detail.push(format!(
                "kernel module {} ({}) · libraries {} ({} → {})",
                kernel.version,
                flavor(kernel.flavor),
                userspace.version,
                userspace.link.display(),
                userspace.target.display()
            ));
            by_nvml(c, detail)
        }
        DriverState::UserspaceUnreadable { kernel, why } => {
            detail.push(format!(
                "kernel module {} ({}) · libraries' version not readable: {why}; NVML's own \
                 start compares them",
                kernel.version,
                flavor(kernel.flavor),
            ));
            by_nvml(c, detail)
        }
        DriverState::Differ { kernel, userspace } => {
            detail.push(format!(
                "kernel module {} ({}) · libraries on disk {} ({} → {})",
                kernel.version,
                lmgw_machine::driver::PROC_NVIDIA_VERSION,
                userspace.version,
                userspace.link.display(),
                userspace.target.display()
            ));
            detail.push("NVML not asked: it would answer for the old module".into());
            (
                item(
                    id,
                    ItemState::Problem,
                    "Restart the computer to finish the graphics driver update.",
                    detail,
                    Some(MachineCause::DriverMismatch {
                        kernel: kernel.version.clone(),
                        userspace: userspace.version.clone(),
                    }),
                ),
                Card::NeedsRestart,
            )
        }
        DriverState::NoModule { userspace } => no_module(c, userspace, detail),
        DriverState::NoLibraries { kernel } => {
            detail.push(format!(
                "kernel module {} running · no {} in {}",
                kernel.version,
                lmgw_machine::driver::NVML_SONAME,
                lmgw_machine::driver::NVML_LIB_DIRS.join(", ")
            ));
            detail.push(
                "on Fedora with RPM Fusion's driver: sudo dnf install xorg-x11-drv-nvidia-cuda-libs"
                    .into(),
            );
            (
                item(
                    id,
                    ItemState::Problem,
                    "A part of the graphics driver that local models need is missing. Here's how \
                     to add it.",
                    detail,
                    Some(MachineCause::DriverLibrariesMissing),
                ),
                Card::Problem,
            )
        }
        DriverState::Unreadable(why) => {
            detail.push(why.clone());
            (
                item(
                    id,
                    ItemState::Problem,
                    "lmgw couldn't read which graphics driver is running.",
                    detail,
                    Some(MachineCause::DriverUnreadable { why: why.clone() }),
                ),
                Card::Problem,
            )
        }
    }
}

/// No NVIDIA module runs (§1.1): not installed, not built for this kernel
/// yet, built after this boot started, or kept from loading by Secure Boot.
/// Secure Boot is blamed only when it is on, a module is built for the
/// running kernel, and this boot started after that module was put there:
/// otherwise the module may still be building (akmods builds it in the
/// background after a kernel or driver update) or the next boot loads it.
fn no_module(
    c: &CardFacts,
    userspace: &Read<lmgw_machine::driver::Userspace>,
    mut detail: Vec<String>,
) -> (MachineItem, Card) {
    let id = MachineItemId::GraphicsCard;
    let installed = matches!(userspace, Read::Found(_))
        || c.driver_packages.iter().any(|p| {
            p.name == "xorg-x11-drv-nvidia" || p.name == "akmod-nvidia" || p.name == "kmod-nvidia"
        });
    let bound: Option<String> = c
        .pci
        .iter()
        .find(|d| d.vendor() == Vendor::Nvidia)
        .and_then(|d| d.driver.clone());
    detail.push(format!(
        "no NVIDIA kernel module running ({} is not there)",
        lmgw_machine::driver::PROC_NVIDIA_VERSION
    ));
    if !installed {
        detail.push(
            "how Fedora installs it: turn on third-party software sources in Discover \
             (or GNOME Software) and install the NVIDIA driver, then restart; or RPM \
             Fusion's commands: sudo dnf install akmod-nvidia, then wait until \
             modinfo -F version nvidia names a version, then restart \
             (https://rpmfusion.org/Howto/NVIDIA)"
                .into(),
        );
        return (
            item(
                id,
                ItemState::Problem,
                "Your NVIDIA card is using the basic built-in driver. Local models need \
                 NVIDIA's own driver.",
                detail,
                Some(MachineCause::NoNvidiaDriver { bound }),
            ),
            Card::NeedsDriver,
        );
    }
    detail.push(format!(
        "Secure Boot: {}",
        match &c.secure_boot {
            SecureBoot::On => "on".to_string(),
            SecureBoot::Off => "off".to_string(),
            SecureBoot::NotEfi => "not an EFI boot".to_string(),
            SecureBoot::Unknown(why) => format!("unknown ({why})"),
        }
    ));
    let booted_after = match (&c.module, c.boot_ns) {
        (Some(ModuleFile::Built(m)), boot) => {
            detail.push(format!(
                "built for kernel {}: {}, put there {} UTC{}",
                m.release,
                m.path.display(),
                utc(m.installed_ns),
                match boot {
                    Some(b) => format!("; this boot started {} UTC", utc(b)),
                    None => "; when this boot started could not be read".into(),
                }
            ));
            boot.map(|b| b > m.installed_ns)
        }
        (Some(ModuleFile::NotBuilt { release }), _) => {
            detail.push(format!(
                "no nvidia.ko for kernel {release} yet (looked in {}/{release}/{{{}}})",
                lmgw_machine::driver::MODULES_DIR,
                lmgw_machine::driver::MODULE_SUBDIRS.join(",")
            ));
            None
        }
        (Some(ModuleFile::Unknown(why)), _) => {
            detail.push(format!("which kernel runs could not be read: {why}"));
            None
        }
        (None, _) => None,
    };
    let secure_boot = c.secure_boot == SecureBoot::On && booted_after == Some(true);
    let sentence = if secure_boot {
        detail.push(
            "RPM Fusion's Secure Boot steps: https://rpmfusion.org/Howto/Secure%20Boot".into(),
        );
        "The graphics driver is installed, but Secure Boot stops it from loading. It needs a \
         one-time key enrolment."
    } else if booted_after == Some(false) {
        "The graphics driver is ready but not running yet. Restart the computer to start it."
    } else {
        "The graphics driver is installed but not running yet. It may still be getting ready \
         for this computer: wait a few minutes, then restart the computer."
    };
    (
        item(
            id,
            ItemState::Problem,
            sentence,
            detail,
            Some(MachineCause::DriverNotRunning { secure_boot }),
        ),
        if secure_boot {
            Card::Problem
        } else {
            Card::NeedsRestart
        },
    )
}

fn amd(c: &CardFacts, mut detail: Vec<String>) -> (MachineItem, Card) {
    let id = MachineItemId::GraphicsCard;
    let bound = c
        .pci
        .iter()
        .any(|d| d.vendor() == Vendor::Amd && d.driver.as_deref() == Some("amdgpu"));
    detail.push(format!(
        "{KFD}: {} · render nodes in {DRI}: {}",
        if c.kfd {
            "there"
        } else {
            "not there (Vulkan engines compute without it; ROCm ones need it)"
        },
        if c.render_nodes.is_empty() {
            "none".to_string()
        } else {
            c.render_nodes.join(", ")
        }
    ));
    let cards = match &c.amd {
        Some(Ok(cards)) => {
            for k in cards {
                detail.push(format!(
                    "amdgpu sysfs: card {} {} · {} total, {} used",
                    k.index,
                    k.name,
                    fmt_bytes(k.total_bytes),
                    fmt_bytes(k.used_bytes)
                ));
            }
            cards.clone()
        }
        Some(Err(e)) => {
            detail.push(format!("amdgpu sysfs counters: {e}"));
            Vec::new()
        }
        None => Vec::new(),
    };
    // The render nodes are what a Vulkan engine computes through, and what
    // lmgw's settings for AMD cards give models; `/dev/kfd` is ROCm's alone,
    // and a ROCm setting without it is §1.4's to judge (D137).
    if !bound || c.render_nodes.is_empty() {
        return (
            item(
                id,
                ItemState::Problem,
                "lmgw can't compute on this AMD card: its driver isn't giving models access to it.",
                detail,
                Some(MachineCause::AmdNoComputeNode),
            ),
            Card::Problem,
        );
    }
    let name = c
        .pci
        .iter()
        .find(|d| d.vendor() == Vendor::Amd)
        .map(device_name)
        .unwrap_or_else(|| "AMD".into());
    let sentence = match cards.first() {
        Some(k) => format!("Graphics card · {name} · {}", fmt_bytes(k.total_bytes)),
        None => format!("Graphics card · {name}"),
    };
    (
        item(id, ItemState::Ok, &sentence, detail, None),
        Card::AmdOk,
    )
}
