//! The Windows probe: NVML for the devices, D3DKMT for the processes.

use super::{d3dkmt, processes, PciAddress};
use crate::vram::nvml::{GpuMemory, GpuPower, GpuProbe, NvmlProbe, ProcessMemory};

/// NVML's device totals, power and driver version, with the per-process
/// figures NVML cannot give under WDDM read from D3DKMT ([`super`]).
pub struct WddmProbe {
    nvml: NvmlProbe,
}

impl WddmProbe {
    pub fn new(nvml: NvmlProbe) -> Self {
        Self { nvml }
    }

    fn devices_pci(&self) -> Result<Vec<PciAddress>, String> {
        self.nvml.pci_addresses()
    }
}

impl GpuProbe for WddmProbe {
    fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        self.nvml.devices()
    }

    fn source(&self) -> String {
        format!("{}, per process from D3DKMT", self.nvml.source())
    }

    /// Only whether the sources are there: NVML loaded and the D3DKMT
    /// statistics callable (one count call). It is asked on async threads
    /// and before every pass; which adapters count, and an NVML device
    /// without its adapter, are [`Self::processes`]'s to find, on the
    /// blocking thread a pass reads on.
    fn process_support(&self) -> Result<(), String> {
        self.nvml.process_support()?;
        d3dkmt::available()
    }

    fn processes(&self, own: &[u32], retired: &[u32]) -> Result<Vec<ProcessMemory>, String> {
        processes(&d3dkmt::Real, &self.devices_pci()?, own, retired)
    }

    fn power(&self) -> Result<Vec<GpuPower>, String> {
        self.nvml.power()
    }

    fn driver_version(&self) -> Option<String> {
        self.nvml.driver_version()
    }
}
