//! The display adapters Windows has a driver for (Windows design §4.4,
//! WP6): what the Linux check reads from the PCI bus in sysfs (`pci`), read
//! from the display class's driver keys in the registry,
//! `HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-…}\NNNN`. Each
//! key is one installed adapter: its name (`DriverDesc`), the driver's
//! provider and version, the PCI vendor and device in `MatchingDeviceId`
//! (`PCI\VEN_10DE&DEV_13C2…`), and the memory size the driver reports
//! (`HardwareInformation.qwMemorySize`, or the 32-bit `…MemorySize`). Every
//! user may read these keys; the check needs no rights of its own.
//!
//! A key outlives its device (a card taken out, a VM's earlier display), so
//! only the keys of the display devices SetupAPI lists as present
//! (`DIGCF_PRESENT`, each device's driver key `SPDRP_DRIVER`) are read.
//!
//! The adapter list is vendor-neutral: NVIDIA, AMD and Intel cards are
//! listed alike, and so is Microsoft's basic display adapter of a machine or
//! VM without a card driver (`ROOT\BasicDisplay`, no PCI ids:
//! [`Adapter::is_basic`]). Read on a Windows 11 test machine (2026-11-10): `0000` the basic adapter (Microsoft, 10.0.26100.1), `0001` a GTX 970 test card (NVIDIA 32.0.15.6081, PCI 10de:13c2, 4 GiB).

#[cfg(windows)]
#[path = "display/windows.rs"]
mod registry;
#[cfg(windows)]
pub use registry::adapters;

/// The display class's key.
pub const CLASS_KEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";

/// One adapter's driver key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Adapter {
    /// The key's name under the class (`0000`).
    pub key: String,
    /// `DriverDesc`: "NVIDIA GeForce GTX 970".
    pub name: String,
    /// `ProviderName`: "NVIDIA".
    pub provider: Option<String>,
    /// `DriverVersion`, Windows' four-part form: "32.0.15.6081".
    pub driver_version: Option<String>,
    /// The PCI vendor ID from `MatchingDeviceId`.
    pub vendor: Option<u16>,
    /// The PCI device ID from `MatchingDeviceId`.
    pub device: Option<u16>,
    /// What the driver reports as the adapter's memory, in bytes.
    pub memory_bytes: Option<u64>,
}

impl Adapter {
    /// Microsoft's basic display adapter, the driver Windows uses for a card
    /// it has no driver for (and a VM's display): no PCI ids
    /// (`ROOT\BasicDisplay`), Microsoft's driver, or Microsoft's vendor ID.
    pub fn is_basic(&self) -> bool {
        self.vendor == Some(VENDOR_MICROSOFT)
            || (self.vendor.is_none() && self.provider.as_deref() == Some("Microsoft"))
    }
}

/// PCI vendor IDs the check names.
pub const VENDOR_NVIDIA: u16 = 0x10de;
pub const VENDOR_AMD: u16 = 0x1002;
pub const VENDOR_INTEL: u16 = 0x8086;
/// Microsoft's basic display adapter (no card driver).
pub const VENDOR_MICROSOFT: u16 = 0x1414;

/// `VEN_xxxx` and `DEV_xxxx` of a `MatchingDeviceId`, case-insensitive.
pub fn pci_ids(matching: &str) -> (Option<u16>, Option<u16>) {
    let upper = matching.to_ascii_uppercase();
    let id = |tag: &str| {
        upper.find(tag).and_then(|i| {
            let hex = upper.get(i + tag.len()..i + tag.len() + 4)?;
            u16::from_str_radix(hex, 16).ok()
        })
    };
    (id("VEN_"), id("DEV_"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pci_ids_are_read_from_a_matching_device_id() {
        assert_eq!(
            pci_ids(r"pci\ven_10de&dev_13c2&subsys_00001234"),
            (Some(0x10de), Some(0x13c2))
        );
        assert_eq!(pci_ids("ROOT\\BasicDisplay"), (None, None));
    }

    /// This machine's adapters read without rights of their own: at least
    /// one is listed (a VM without a card driver has Microsoft's basic one).
    #[cfg(windows)]
    #[test]
    fn this_machines_adapters_are_read() {
        let found = adapters().unwrap();
        assert!(!found.is_empty(), "no display adapter in the registry");
        for a in &found {
            println!("{a:?}");
        }
    }
}
