//! A stated NVIDIA machine, shared by the suites.

use lmgw_machine::cdi::spec::{Format, Spec};
use lmgw_machine::fake::FakeFacts;

/// A spec as `nvidia-ctk cdi generate` writes it rootless (toolkit 1.20.1,
/// driver 600.20.03), with a made-up GPU UUID and PCI address: YAML, as the
/// system's specs are.
pub const SPEC: &str = include_str!("../fixtures/nvidia.yaml");

/// The same spec as JSON, as lmgw writes its own (`--format=json`).
pub const OWN_SPEC: &str = include_str!("../fixtures/nvidia.json");

/// The open module's `/proc/driver/nvidia/version`, at a given version.
pub fn open_module_text(version: &str) -> String {
    let f = FakeFacts::new();
    f.set_kernel_module(version);
    String::from_utf8(
        lmgw_machine::Facts::read(&f, std::path::Path::new("/proc/driver/nvidia/version")).unwrap(),
    )
    .unwrap()
}

/// The proprietary module's line, as 550.54.14 writes it.
pub fn proprietary_module_text(version: &str) -> String {
    format!(
        "NVRM version: NVIDIA UNIX x86_64 Kernel Module  {version}  Thu Feb 22 01:44:30 UTC \
         2024\nGCC version:  gcc version 13.2.1 20231205 (Red Hat 13.2.1-6) (GCC) \n"
    )
}

/// The running kernel module at `version`.
pub fn set_module(f: &FakeFacts, version: &str) {
    f.set_kernel_module(version);
}

/// The userspace libraries at `version`: the soname link and its target.
pub fn set_libraries(f: &FakeFacts, version: &str) {
    f.set_driver_libraries(version);
}

/// Every host path the fixture spec names, present.
pub fn add_spec_paths(f: &FakeFacts) {
    let spec = Spec::parse(SPEC.as_bytes(), Format::Yaml).expect("the fixture parses");
    for p in spec.named_paths() {
        f.file(&p, b"");
    }
}

/// An NVIDIA machine at driver `version` with the toolkit, an rpm database,
/// and everything the fixture spec names.
pub fn nvidia_machine(version: &str) -> FakeFacts {
    let f = FakeFacts::nvidia(version);
    add_spec_paths(&f);
    f
}
