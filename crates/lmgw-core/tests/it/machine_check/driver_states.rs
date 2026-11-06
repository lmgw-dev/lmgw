//! §1.1's driver states the review round settled: a library version that
//! cannot be read leaves the decision to NVML (D26), NVML's own
//! `version_mismatch` is a restart, and Secure Boot is blamed for a module
//! that does not load only when one is built and this boot came after it.

use lmgw_api_types::machine::{ItemState, MachineCause, MachineItemId, MachineVerdict};

use crate::support::machine_world::World;

fn item(
    c: &lmgw_api_types::machine::MachineCheck,
    id: MachineItemId,
) -> &lmgw_api_types::machine::MachineItem {
    c.items.iter().find(|i| i.id == id).unwrap()
}

/// The running kernel, this boot's start (2026-01-01 08:00:00 UTC) and, as
/// `module_built_*` adds it, akmods' module for that kernel.
const RELEASE: &str = "6.99.1-100.fc44.x86_64";
const BOOT_S: i128 = 1_767_254_400;
const KO: &str = "/lib/modules/6.99.1-100.fc44.x86_64/extra/nvidia/nvidia.ko.xz";

fn kernel(w: &World) {
    w.m.facts
        .file(lmgw_machine::driver::OSRELEASE, format!("{RELEASE}\n"))
        .file(
            lmgw_machine::driver::PROC_STAT,
            format!("cpu  1 2 3\nbtime {BOOT_S}\nprocesses 1\n"),
        );
}

/// The module built and put there 15 minutes before this boot (its
/// modification time the package's, a month earlier).
pub fn module_built_before_boot(w: &World) {
    kernel(w);
    w.m.facts
        .file(KO, b"")
        .touch(KO, (BOOT_S - 30 * 86_400) * 1_000_000_000)
        .touch_ctime(KO, (BOOT_S - 900) * 1_000_000_000);
}

/// An unversioned `libnvidia-ml.so.1` target while the module runs: NVML is
/// asked, its answer decides (ready here), the unreadable version is a
/// detail line, and the boot run writes lmgw's spec as on any working card.
#[tokio::test]
async fn an_unversioned_library_is_decided_by_nvml() {
    let w = World::nvidia().await;
    w.all_images_here();
    w.m.facts
        .file("/usr/lib64/libnvidia-ml.so", b"\x7fELF")
        .link("/usr/lib64/libnvidia-ml.so.1", "libnvidia-ml.so");
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::Ready, "{c:#?}");
    assert_eq!(w.probes(), 1, "NVML asked");
    let card = item(&c, MachineItemId::GraphicsCard);
    assert!(
        card.detail
            .iter()
            .any(|l| l.contains("libraries' version not readable")
                && l.contains("carries no driver version")),
        "{:?}",
        card.detail
    );
    lmgw_core::machine::boot(&w.state).await;
    assert_eq!(w.m.generations(), 1, "the boot run wrote lmgw's spec");
}

/// NVML's own `version_mismatch` (its library and the module disagree,
/// which the unreadable version could not show) is the restart.
#[tokio::test]
async fn nvmls_version_mismatch_needs_a_restart() {
    let w = World::nvidia().await;
    w.m.facts
        .file("/usr/lib64/libnvidia-ml.so", b"\x7fELF")
        .link("/usr/lib64/libnvidia-ml.so.1", "libnvidia-ml.so");
    *w.nvml.lock().unwrap() = Some(lmgw_core::machine::probe::NvmlAnswer::Failed {
        kind: "version_mismatch".into(),
        error: "RM has detected an NVML/RM version mismatch.".into(),
    });
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::NeedsRestart, "{c:#?}");
    assert_eq!(
        c.headline,
        "Restart the computer to finish the graphics driver update."
    );
    assert!(matches!(
        &item(&c, MachineItemId::GraphicsCard).cause,
        Some(MachineCause::NvmlFailed { error }) if error.starts_with("version_mismatch")
    ));
    assert_eq!(
        item(&c, MachineItemId::CardForModels).state,
        ItemState::NotChecked
    );
}

/// Secure Boot on and the module not loaded: blamed only when a module is
/// built for the running kernel and this boot started after it was put
/// there; with none built yet it may still be building, and one built
/// after this boot needs the restart. Secure Boot is a detail line either
/// way.
#[tokio::test]
async fn secure_boot_is_blamed_only_for_a_module_built_before_this_boot() {
    let w = World::nvidia().await;
    w.m.facts
        .remove("/proc/driver/nvidia/version")
        .dir("/sys/firmware/efi")
        .file(lmgw_machine::driver::SECURE_BOOT_VAR, [6u8, 0, 0, 0, 1]);
    kernel(&w);

    // Nothing built for this kernel yet: akmods may still be at it.
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    assert_eq!(
        card.cause,
        Some(MachineCause::DriverNotRunning { secure_boot: false })
    );
    assert_eq!(c.verdict, MachineVerdict::NeedsRestart);
    assert!(c.headline.contains("wait a few minutes"), "{}", c.headline);
    assert!(card.detail.iter().any(|l| l == "Secure Boot: on"));
    assert!(
        card.detail
            .iter()
            .any(|l| l.starts_with(&format!("no nvidia.ko for kernel {RELEASE} yet"))),
        "{:?}",
        card.detail
    );

    // Built after this boot started: the restart loads it.
    w.m.facts
        .file(KO, b"")
        .touch_ctime(KO, (BOOT_S + 600) * 1_000_000_000);
    let c = w.check().await;
    assert_eq!(
        item(&c, MachineItemId::GraphicsCard).cause,
        Some(MachineCause::DriverNotRunning { secure_boot: false })
    );
    assert_eq!(c.verdict, MachineVerdict::NeedsRestart);
    assert!(
        c.headline.contains("Restart the computer to start it"),
        "{}",
        c.headline
    );

    // Built before this boot, and still not loaded: Secure Boot.
    module_built_before_boot(&w);
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    assert_eq!(
        card.cause,
        Some(MachineCause::DriverNotRunning { secure_boot: true })
    );
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    assert!(c.headline.contains("Secure Boot"), "{}", c.headline);
    assert!(
        card.detail
            .iter()
            .any(|l| l.contains(KO) && l.contains("this boot started")),
        "{:?}",
        card.detail
    );

    // The same module with Secure Boot off: not Secure Boot's doing.
    w.m.facts
        .file(lmgw_machine::driver::SECURE_BOOT_VAR, [6u8, 0, 0, 0, 0]);
    let c = w.check().await;
    assert_eq!(
        item(&c, MachineItemId::GraphicsCard).cause,
        Some(MachineCause::DriverNotRunning { secure_boot: false })
    );
}
