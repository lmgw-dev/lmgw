//! The graphics cards on the PCI bus, from sysfs (machine-check design §1.1):
//! every display-class device (`0x03xxxx`), its vendor and device IDs, the
//! kernel driver bound to it, and its names from hwdata's `pci.ids`.

use std::path::Path;

use crate::facts::{read_text, Facts};

/// The PCI devices sysfs lists, one directory per bus address.
pub const PCI_DEVICES: &str = "/sys/bus/pci/devices";

/// Where `pci.ids` is: hwdata's path (Fedora), then the older one Debian
/// keeps.
pub const PCI_IDS: [&str; 2] = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"];

pub const VENDOR_NVIDIA: u16 = 0x10de;
pub const VENDOR_AMD: u16 = 0x1002;
pub const VENDOR_INTEL: u16 = 0x8086;

/// Who made a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Other(u16),
}

impl Vendor {
    pub fn of(id: u16) -> Self {
        match id {
            VENDOR_NVIDIA => Self::Nvidia,
            VENDOR_AMD => Self::Amd,
            VENDOR_INTEL => Self::Intel,
            other => Self::Other(other),
        }
    }
}

/// One display-class PCI device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciDevice {
    /// The bus address, e.g. `0000:01:00.0`.
    pub address: String,
    /// The 24-bit class code, e.g. `0x030000`.
    pub class: u32,
    pub vendor_id: u16,
    pub device_id: u16,
    /// The kernel driver bound to it (`nvidia`, `nouveau`, `amdgpu`, `i915`),
    /// `None` when none is.
    pub driver: Option<String>,
    /// From `pci.ids`, when it knows them.
    pub vendor_name: Option<String>,
    pub device_name: Option<String>,
}

impl PciDevice {
    pub fn vendor(&self) -> Vendor {
        Vendor::of(self.vendor_id)
    }
}

fn hex_u32(text: &str) -> Option<u32> {
    let t = text.trim();
    u32::from_str_radix(t.strip_prefix("0x").unwrap_or(t), 16).ok()
}

/// Every display-class device sysfs lists, in bus order. A device whose
/// class, vendor or device file cannot be read is left out: it says nothing
/// about a card.
pub fn display_devices(f: &dyn Facts) -> Vec<PciDevice> {
    let root = Path::new(PCI_DEVICES);
    let Ok(addresses) = f.list(root) else {
        return Vec::new();
    };
    let ids = PCI_IDS.iter().find_map(|p| read_text(f, Path::new(p)).ok());
    let mut out = Vec::new();
    for address in addresses {
        let dir = root.join(&address);
        let read = |name: &str| read_text(f, &dir.join(name)).ok().and_then(|t| hex_u32(&t));
        let Some(class) = read("class") else { continue };
        if class >> 16 != 0x03 {
            continue;
        }
        let (Some(vendor), Some(device)) = (read("vendor"), read("device")) else {
            continue;
        };
        let (vendor_id, device_id) = (vendor as u16, device as u16);
        let driver = f
            .read_link(&dir.join("driver"))
            .ok()
            .and_then(|t| t.file_name().map(|n| n.to_string_lossy().into_owned()));
        let (vendor_name, device_name) = ids
            .as_deref()
            .map(|ids| names(ids, vendor_id, device_id))
            .unwrap_or((None, None));
        out.push(PciDevice {
            address,
            class,
            vendor_id,
            device_id,
            driver,
            vendor_name,
            device_name,
        });
    }
    out
}

/// The vendor's and the device's names in `pci.ids`: a vendor line is
/// `vvvv  Name` at column 0, its devices follow as `\tdddd  Name`, and
/// subsystem lines (two tabs) are skipped.
pub fn names(ids: &str, vendor: u16, device: u16) -> (Option<String>, Option<String>) {
    let vendor_hex = format!("{vendor:04x}");
    let device_hex = format!("{device:04x}");
    let mut vendor_name = None;
    for line in ids.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if !line.starts_with('\t') {
            if vendor_name.is_some() {
                // The next vendor: the device is not listed.
                break;
            }
            if let Some(rest) = line
                .strip_prefix(&vendor_hex)
                .filter(|r| r.starts_with(' '))
            {
                vendor_name = Some(rest.trim().to_string());
            }
            continue;
        }
        if vendor_name.is_none() || line.starts_with("\t\t") {
            continue;
        }
        if let Some(rest) = line[1..]
            .strip_prefix(&device_hex)
            .filter(|r| r.starts_with(' '))
        {
            return (vendor_name, Some(rest.trim().to_string()));
        }
    }
    (vendor_name, None)
}
