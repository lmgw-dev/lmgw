//! The driver facts (machine-check design §6, "Driver facts"): the module's
//! `/proc` line, the libraries' link, the two compared, and Secure Boot.

use lmgw_machine::driver::{
    kernel_module, parse_nvrm, secure_boot, userspace, version_of_library, DriverState,
    ModuleFlavor, Read, SecureBoot, SECURE_BOOT_VAR,
};
use lmgw_machine::fake::FakeFacts;

use crate::support::{open_module_text, proprietary_module_text, set_libraries, set_module};

#[test]
fn the_open_module_line_gives_its_version_and_flavor() {
    let m = parse_nvrm(&open_module_text("600.20.03")).expect("parses");
    assert_eq!(m.version, "600.20.03");
    assert_eq!(m.flavor, ModuleFlavor::Open);
    assert!(m.line.starts_with("NVRM version:"), "{}", m.line);
}

#[test]
fn the_proprietary_module_line_gives_its_version_and_flavor() {
    let m = parse_nvrm(&proprietary_module_text("550.54.14")).expect("parses");
    assert_eq!(m.version, "550.54.14");
    assert_eq!(m.flavor, ModuleFlavor::Proprietary);
}

#[test]
fn a_line_without_a_version_is_no_module_line() {
    assert_eq!(parse_nvrm("NVRM version: something else\n"), None);
    assert_eq!(parse_nvrm("GCC version: 16.2.1\n"), None);
}

#[test]
fn the_library_link_gives_the_userspace_version() {
    let f = FakeFacts::new();
    set_libraries(&f, "600.20.03");
    let Read::Found(u) = userspace(&f) else {
        panic!("{:?}", userspace(&f))
    };
    assert_eq!(u.version, "600.20.03");
    assert_eq!(u.link.to_str(), Some("/usr/lib64/libnvidia-ml.so.1"));
    assert_eq!(
        u.target.to_str(),
        Some("/usr/lib64/libnvidia-ml.so.600.20.03")
    );
    assert_eq!(
        version_of_library(std::path::Path::new("libnvidia-ml.so.1")),
        None,
        "the soname itself carries no driver version"
    );
}

#[test]
fn the_debian_multiarch_dir_is_found_too() {
    let f = FakeFacts::new();
    f.file("/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.550.54.14", b"")
        .link(
            "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1",
            "libnvidia-ml.so.550.54.14",
        );
    assert!(matches!(userspace(&f), Read::Found(u) if u.version == "550.54.14"));
}

#[test]
fn nixos_driver_dir_is_found_too() {
    let f = FakeFacts::new();
    f.file("/run/opengl-driver/lib/libnvidia-ml.so.600.20.03", b"")
        .link(
            "/run/opengl-driver/lib/libnvidia-ml.so.1",
            "libnvidia-ml.so.600.20.03",
        );
    assert!(matches!(userspace(&f), Read::Found(u) if u.version == "600.20.03"));
}

#[test]
fn a_dangling_library_link_is_unreadable_not_absent() {
    let f = FakeFacts::new();
    f.link("/usr/lib64/libnvidia-ml.so.1", "libnvidia-ml.so.600.20.03");
    assert!(matches!(userspace(&f), Read::Unreadable(why) if why.contains("points nowhere")));
}

#[test]
fn no_library_is_absent() {
    assert_eq!(userspace(&FakeFacts::new()), Read::Absent);
}

#[test]
fn module_and_libraries_at_one_version_agree() {
    let f = FakeFacts::new();
    set_module(&f, "600.20.03");
    set_libraries(&f, "600.20.03");
    let state = DriverState::read(&f);
    assert_eq!(state.agreed_version(), Some("600.20.03"));
}

#[test]
fn lmgw_running_across_a_driver_update_sees_the_restart_and_then_the_new_driver() {
    // Before: module and libraries at 600.10.01.
    let f = FakeFacts::new();
    set_module(&f, "600.10.01");
    set_libraries(&f, "600.10.01");
    assert_eq!(DriverState::read(&f).agreed_version(), Some("600.10.01"));

    // The update replaces the libraries on disk; the old module keeps running.
    f.remove("/usr/lib64/libnvidia-ml.so.600.10.01");
    set_libraries(&f, "600.20.03");
    match DriverState::read(&f) {
        DriverState::Differ { kernel, userspace } => {
            assert_eq!(kernel.version, "600.10.01");
            assert_eq!(userspace.version, "600.20.03");
        }
        other => panic!("{other:?}"),
    }

    // The restart loads the new module.
    set_module(&f, "600.20.03");
    assert_eq!(DriverState::read(&f).agreed_version(), Some("600.20.03"));
}

#[test]
fn no_module_and_no_libraries_are_told_apart() {
    let f = FakeFacts::new();
    set_libraries(&f, "600.20.03");
    assert!(matches!(
        DriverState::read(&f),
        DriverState::NoModule {
            userspace: Read::Found(_)
        }
    ));
    let g = FakeFacts::new();
    set_module(&g, "600.20.03");
    assert!(matches!(
        DriverState::read(&g),
        DriverState::NoLibraries { kernel } if kernel.version == "600.20.03"
    ));
    assert_eq!(kernel_module(&FakeFacts::new()), Read::Absent);
}

#[test]
fn an_unparsable_proc_file_is_unreadable() {
    let f = FakeFacts::new();
    f.file("/proc/driver/nvidia/version", "garbage\n");
    set_libraries(&f, "600.20.03");
    assert!(
        matches!(DriverState::read(&f), DriverState::Unreadable(why) if why.contains("garbage"))
    );
}

#[test]
fn secure_boot_reads_the_data_byte_after_the_attributes() {
    let on = FakeFacts::new();
    on.file(SECURE_BOOT_VAR, [0x06, 0, 0, 0, 1]);
    assert_eq!(secure_boot(&on), SecureBoot::On);

    let off = FakeFacts::new();
    off.file(SECURE_BOOT_VAR, [0x06, 0, 0, 0, 0]);
    assert_eq!(secure_boot(&off), SecureBoot::Off);

    let efi_without_var = FakeFacts::new();
    efi_without_var.dir("/sys/firmware/efi/efivars");
    assert_eq!(secure_boot(&efi_without_var), SecureBoot::Off);

    assert_eq!(secure_boot(&FakeFacts::new()), SecureBoot::NotEfi);

    let short = FakeFacts::new();
    short.file(SECURE_BOOT_VAR, [0x06, 0, 0]);
    assert!(matches!(secure_boot(&short), SecureBoot::Unknown(_)));
}

/// The module runs and `libnvidia-ml.so.1` points at a file whose name
/// carries no version (a distribution's unversioned library): NVML is the
/// one to say whether the halves match, and lmgw's spec is not blocked.
#[test]
fn an_unversioned_library_leaves_the_decision_to_nvml() {
    let f = FakeFacts::new();
    set_module(&f, "600.20.03");
    f.file("/usr/lib64/libnvidia-ml.so", b"\x7fELF")
        .link("/usr/lib64/libnvidia-ml.so.1", "libnvidia-ml.so");
    let state = DriverState::read(&f);
    match &state {
        DriverState::UserspaceUnreadable { kernel, why } => {
            assert_eq!(kernel.version, "600.20.03");
            assert!(why.contains("carries no driver version"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    assert!(state.nvml_decides());
    assert_eq!(state.agreed_version(), None);
    // A module whose own line cannot be read is still unreadable as a whole.
    f.file(lmgw_machine::driver::PROC_NVIDIA_VERSION, "garbage\n");
    assert!(matches!(DriverState::read(&f), DriverState::Unreadable(_)));
    assert!(!DriverState::read(&f).nvml_decides());
}

/// The module built for the running kernel, found where akmods puts it,
/// with the time it was put there (its change time, not the package's
/// modification time); none for a kernel it has not been built for yet.
#[test]
fn the_module_file_for_the_running_kernel_and_the_boot_time() {
    use lmgw_machine::driver::{boot_time_ns, module_file, ModuleFile};
    let f = FakeFacts::new();
    assert!(matches!(module_file(&f), ModuleFile::Unknown(_)));
    f.file(lmgw_machine::driver::OSRELEASE, "6.99.1-100.fc44.x86_64\n");
    assert_eq!(
        module_file(&f),
        ModuleFile::NotBuilt {
            release: "6.99.1-100.fc44.x86_64".into()
        }
    );
    let ko = "/lib/modules/6.99.1-100.fc44.x86_64/extra/nvidia/nvidia.ko.xz";
    f.file(ko, b"")
        .file(
            "/lib/modules/6.99.1-100.fc44.x86_64/extra/nvidia/nvidia-drm.ko.xz",
            b"",
        )
        // The package's time, then installed on 2026-01-01 07:45:00 UTC.
        .touch(ko, 1_764_000_000 * 1_000_000_000)
        .touch_ctime(ko, 1_767_253_500 * 1_000_000_000);
    match module_file(&f) {
        ModuleFile::Built(m) => {
            assert_eq!(m.path, std::path::Path::new(ko));
            assert_eq!(m.installed_ns, 1_767_253_500 * 1_000_000_000);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(boot_time_ns(&f), None);
    f.file(
        lmgw_machine::driver::PROC_STAT,
        "cpu  1 2 3\nintr 0\nctxt 9\nbtime 1767254400\nprocesses 1\n",
    );
    assert_eq!(boot_time_ns(&f), Some(1_767_254_400 * 1_000_000_000));
}
