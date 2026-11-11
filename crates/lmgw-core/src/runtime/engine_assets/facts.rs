//! The machine facts the variant choice reads ([`super::choice::GpuFacts`]):
//! NVML's cards with their compute capability, the driver's CUDA version,
//! and whether a Vulkan loader is installed.
//!
//! NVML is loaded in this process, once per read: on Windows a driver update
//! replaces the DLL for the next load, and the stale-mapping problem the
//! Linux machine check works around (a process of its own,
//! `machine::probe`) is the Linux kernel module's.

use super::choice::{Card, GpuFacts};
use super::coverage::Version;

/// Read the facts now (blocking: NVML's load and its calls).
pub fn read() -> GpuFacts {
    let mut facts = GpuFacts {
        vulkan_loader: vulkan_loader(),
        ..GpuFacts::default()
    };
    match nvml_wrapper::Nvml::init() {
        Ok(nvml) => {
            facts.driver_cuda = nvml
                .sys_cuda_driver_version()
                .ok()
                .and_then(Version::from_nvml);
            match nvml.device_count() {
                Ok(n) => {
                    for index in 0..n {
                        match nvml.device_by_index(index) {
                            Ok(d) => facts.cards.push(Card {
                                index,
                                name: d.name().unwrap_or_else(|_| format!("GPU {index}")),
                                cc: d.cuda_compute_capability().ok().and_then(|c| {
                                    Some(Version::new(
                                        u32::try_from(c.major).ok()?,
                                        u32::try_from(c.minor).ok()?,
                                    ))
                                }),
                            }),
                            Err(e) => {
                                facts.nvml_error =
                                    Some(format!("nvmlDeviceGetHandleByIndex({index}): {e}"))
                            }
                        }
                    }
                }
                Err(e) => facts.nvml_error = Some(format!("nvmlDeviceGetCount: {e}")),
            }
        }
        Err(e) => facts.nvml_error = Some(format!("NVML: {e}")),
    }
    facts
}

/// The Vulkan loader is installed: `vulkan-1.dll` in Windows' system
/// directory, `libvulkan.so.1` in a usual library directory elsewhere.
fn vulkan_loader() -> bool {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        std::path::Path::new(&root)
            .join("System32")
            .join("vulkan-1.dll")
            .is_file()
    }
    #[cfg(not(windows))]
    {
        [
            "/usr/lib64/libvulkan.so.1",
            "/usr/lib/x86_64-linux-gnu/libvulkan.so.1",
            "/usr/lib/aarch64-linux-gnu/libvulkan.so.1",
            "/usr/lib/libvulkan.so.1",
        ]
        .iter()
        .any(|p| std::path::Path::new(p).is_file())
    }
}
