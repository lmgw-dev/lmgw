//! The judgement of lmgw's own spec (machine-check design §6, "lmgw's own
//! spec"; §3.6.2, D20 and D26): fresh, each reason it is stale, and why it
//! cannot be written.

use std::io;
use std::path::PathBuf;

use lmgw_machine::cdi::own::{
    judge, Blocked, Current, Judgement, OnDisk, Stale, Stamp, BOOT_ID, OWN_SPEC_FILE, STAMP_SCHEMA,
};
use lmgw_machine::fake::FakeFacts;

use crate::support::{nvidia_machine, set_libraries, set_module, OWN_SPEC};

/// A stamp written for `f` as it is now.
fn stamp_now(f: &FakeFacts) -> Vec<u8> {
    let stamp: Stamp = Current::read(f)
        .stamp(Some("1.20.1".into()), "2026-01-01 00:00:00".into())
        .expect("a driver that may be written for, and a toolkit");
    serde_json::to_vec(&stamp).unwrap()
}

fn missing<T>() -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::NotFound, "gone"))
}

/// lmgw's spec dir holding its spec alone, and `stamp` beside it.
fn on_disk(stamp: io::Result<Vec<u8>>) -> OnDisk {
    OnDisk {
        spec: Ok(OWN_SPEC.as_bytes().to_vec()),
        stamp,
        spec_dir: Ok(vec![OWN_SPEC_FILE.to_string()]),
    }
}

fn stale(j: Judgement) -> Vec<Stale> {
    match j {
        Judgement::Stale(r) => r,
        other => panic!("expected stale, got {other:?}"),
    }
}

#[test]
fn a_current_spec_and_stamp_are_fresh() {
    let f = nvidia_machine("600.20.03");
    let stamp = stamp_now(&f);
    assert_eq!(judge(&f, on_disk(Ok(stamp))), Judgement::Fresh);
}

#[test]
fn no_spec_and_no_stamp_are_both_named() {
    let f = nvidia_machine("600.20.03");
    let nothing = OnDisk {
        spec: missing(),
        stamp: missing(),
        spec_dir: missing(),
    };
    assert_eq!(
        stale(judge(&f, nothing)),
        vec![Stale::NoSpec, Stale::NoStamp]
    );
}

#[test]
fn a_gone_host_path_makes_it_stale_and_names_the_path() {
    let f = nvidia_machine("600.20.03");
    let stamp = stamp_now(&f);
    // An egl-wayland update: the old library deleted, nothing else.
    f.remove("/usr/lib64/libnvidia-egl-wayland.so.1.1.24");
    let reasons = stale(judge(&f, on_disk(Ok(stamp))));
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    let Stale::PathsGone(gone) = &reasons[0] else {
        panic!("{reasons:?}")
    };
    assert_eq!(
        gone[0].0,
        PathBuf::from("/usr/lib64/libnvidia-egl-wayland.so.1.1.24")
    );
    assert!(reasons[0]
        .to_string()
        .contains("libnvidia-egl-wayland.so.1.1.24"));
}

#[test]
fn a_package_transaction_makes_it_stale() {
    let f = nvidia_machine("600.20.03");
    let stamp = stamp_now(&f);
    f.touch(
        "/usr/lib/sysimage/rpm/rpmdb.sqlite-wal",
        5_000_000_000_000_000_000,
    );
    assert_eq!(stale(judge(&f, on_disk(Ok(stamp)))), vec![Stale::Packages]);
}

#[test]
fn a_read_of_the_database_does_not() {
    let f = nvidia_machine("600.20.03");
    let stamp = stamp_now(&f);
    f.touch(
        "/usr/lib/sysimage/rpm/rpmdb.sqlite-shm",
        5_000_000_000_000_000_000,
    );
    assert_eq!(judge(&f, on_disk(Ok(stamp))), Judgement::Fresh);
}

#[test]
fn a_replaced_nvidia_ctk_makes_it_stale() {
    let f = nvidia_machine("600.20.03");
    let stamp = stamp_now(&f);
    f.replace_inode("/usr/bin/nvidia-ctk");
    assert_eq!(
        stale(judge(&f, on_disk(Ok(stamp)))),
        vec![Stale::Toolkit {
            was: Some("1.20.1".into())
        }]
    );
}

#[test]
fn a_new_device_node_makes_it_stale_and_names_both_sets() {
    let f = nvidia_machine("600.20.03");
    f.remove("/dev/nvidia-uvm").remove("/dev/nvidia-uvm-tools");
    let stamp = stamp_now(&f);
    // A best-effort `nvidia-modprobe -u` created the uvm nodes later, and a
    // second card was plugged in.
    f.file("/dev/nvidia-uvm", b"")
        .file("/dev/nvidia-uvm-tools", b"")
        .file("/dev/nvidia1", b"");
    let reasons = stale(judge(&f, on_disk(Ok(stamp))));
    let [Stale::DeviceNodes { was, now }] = reasons.as_slice() else {
        panic!("{reasons:?}")
    };
    assert_eq!(was, &["nvidia-modeset", "nvidia0", "nvidiactl"]);
    assert_eq!(
        now,
        &[
            "nvidia-modeset",
            "nvidia-uvm",
            "nvidia-uvm-tools",
            "nvidia0",
            "nvidia1",
            "nvidiactl"
        ]
    );
}

#[test]
fn a_reboot_makes_it_stale() {
    let f = nvidia_machine("600.20.03");
    let stamp = stamp_now(&f);
    f.set_boot_id("0f6c1e9a-5b7d-4e2a-9c3f-000000000002");
    assert_eq!(stale(judge(&f, on_disk(Ok(stamp)))), vec![Stale::Reboot]);
    // The stamp records the ID without its newline.
    let s: Stamp = serde_json::from_slice(&stamp_now(&f)).unwrap();
    assert_eq!(
        s.boot_id.as_deref(),
        Some("0f6c1e9a-5b7d-4e2a-9c3f-000000000002")
    );
    assert!(BOOT_ID.starts_with("/proc/"));
}

#[test]
fn another_file_in_the_spec_dir_makes_it_stale() {
    // Podman reads every *.yaml and *.json in a spec dir: the YAML spec an
    // earlier lmgw wrote, or anything else, would be read beside lmgw's.
    let f = nvidia_machine("600.20.03");
    let stamp = stamp_now(&f);
    let mut disk = on_disk(Ok(stamp));
    disk.spec_dir = Ok(vec![
        "nvidia.json".into(),
        "nvidia.yaml".into(),
        "other.json".into(),
    ]);
    assert_eq!(
        stale(judge(&f, disk)),
        vec![Stale::ExtraFiles(vec![
            "nvidia.yaml".into(),
            "other.json".into()
        ])]
    );
}

#[test]
fn a_stamp_for_another_driver_version_is_stale_on_both_halves() {
    let old = nvidia_machine("600.10.01");
    let stamp = stamp_now(&old);
    let f = nvidia_machine("600.20.03");
    // The same toolkit file and database times as the stamp's machine, so
    // only the driver differs.
    let reasons = stale(judge(&f, on_disk(Ok(stamp))));
    assert!(reasons.contains(&Stale::KernelModule {
        was: Some("600.10.01".into()),
        now: Some("600.20.03".into())
    }));
    assert!(reasons.contains(&Stale::Userspace {
        was: Some("600.10.01".into()),
        now: Some("600.20.03".into())
    }));
}

#[test]
fn while_the_driver_versions_differ_nothing_is_written_whatever_the_spec_says() {
    let f = nvidia_machine("600.10.01");
    let stamp = stamp_now(&f);
    assert_eq!(judge(&f, on_disk(Ok(stamp.clone()))), Judgement::Fresh);
    // The update: new libraries on disk, the old module still running.
    set_libraries(&f, "600.20.03");
    assert_eq!(
        judge(&f, on_disk(Ok(stamp.clone()))),
        Judgement::Blocked(Blocked::Restart {
            kernel: "600.10.01".into(),
            userspace: "600.20.03".into()
        })
    );
    // After the restart: stale for the new driver, so it is written again.
    set_module(&f, "600.20.03");
    let reasons = stale(judge(&f, on_disk(Ok(stamp))));
    assert!(reasons.iter().any(
        |r| matches!(r, Stale::KernelModule { now, .. } if now.as_deref() == Some("600.20.03"))
    ));
}

#[test]
fn no_module_blocks_it() {
    let f = nvidia_machine("600.20.03");
    f.remove("/proc/driver/nvidia/version");
    assert_eq!(
        judge(&f, on_disk(Ok(Vec::new()))),
        Judgement::Blocked(Blocked::NoModule)
    );
    assert_eq!(
        Blocked::NoModule.to_string(),
        "/proc/driver/nvidia/version is not there, so no NVIDIA kernel module is loaded"
    );
}

/// D26: a libraries' version that cannot be read does not keep the spec from
/// being written — nvidia-ctk's own NVML start says when the halves really
/// differ. Neither does a library lmgw finds in none of its dirs.
#[test]
fn an_unreadable_or_missing_library_version_does_not_block_it() {
    // An unversioned library: `libnvidia-ml.so.1` a regular file.
    let f = nvidia_machine("600.20.03");
    f.remove("/usr/lib64/libnvidia-ml.so.1")
        .file("/usr/lib64/libnvidia-ml.so.1", b"\x7fELF");
    let missing_spec = OnDisk {
        spec: missing(),
        stamp: missing(),
        spec_dir: missing(),
    };
    assert_eq!(
        stale(judge(&f, missing_spec)),
        vec![Stale::NoSpec, Stale::NoStamp]
    );
    let stamp: Stamp = serde_json::from_slice(&stamp_now(&f)).unwrap();
    assert_eq!(stamp.kernel_module.as_deref(), Some("600.20.03"));
    assert_eq!(stamp.userspace, None);
    assert!(stamp.nvml.is_some(), "the file is fingerprinted instead");
    // Written once, it is fresh; the file replaced, it is stale.
    assert_eq!(judge(&f, on_disk(Ok(stamp_now(&f)))), Judgement::Fresh);
    let before = stamp_now(&f);
    f.replace_inode("/usr/lib64/libnvidia-ml.so.1");
    assert_eq!(
        stale(judge(&f, on_disk(Ok(before)))),
        vec![Stale::DriverLibrary]
    );

    // No library in any known dir.
    let g = nvidia_machine("600.20.03");
    g.remove("/usr/lib64/libnvidia-ml.so.1");
    assert!(matches!(
        judge(&g, on_disk(Ok(Vec::new()))),
        Judgement::Stale(_)
    ));
}

#[test]
fn a_stale_spec_without_the_toolkit_is_blocked_on_it() {
    let f = nvidia_machine("600.20.03");
    f.uninstall("/usr/bin/nvidia-ctk");
    let nothing = OnDisk {
        spec: missing(),
        stamp: missing(),
        spec_dir: missing(),
    };
    let Judgement::Blocked(Blocked::NoToolkit { stale }) = judge(&f, nothing) else {
        panic!("not blocked on the toolkit")
    };
    assert_eq!(stale, vec![Stale::NoSpec, Stale::NoStamp]);
}

#[test]
fn an_unreadable_stamp_a_spec_of_another_kind_or_an_old_layout_is_stale() {
    let f = nvidia_machine("600.20.03");
    let reasons = stale(judge(&f, on_disk(Ok(b"{not json".to_vec()))));
    assert!(matches!(reasons[0], Stale::StampUnreadable(_)));

    let stamp = stamp_now(&f);
    let mut other = on_disk(Ok(stamp));
    other.spec = Ok(br#"{"cdiVersion":"0.7.0","kind":"example.com/dev","devices":[]}"#.to_vec());
    let reasons = stale(judge(&f, other));
    assert!(matches!(&reasons[0], Stale::SpecUnreadable(why) if why.contains("example.com/dev")));

    // The YAML spec of the first build of this feature is not JSON.
    let mut yaml = on_disk(Ok(stamp_now(&f)));
    yaml.spec = Ok(b"cdiVersion: 0.7.0\nkind: nvidia.com/gpu\ndevices: []\n".to_vec());
    assert!(matches!(
        stale(judge(&f, yaml))[0],
        Stale::SpecUnreadable(_)
    ));

    // A layout-1 stamp says which layout it is.
    let old = br#"{"schema":1,"kernel_module":"600.20.03","userspace":"600.20.03"}"#;
    let reasons = stale(judge(&f, on_disk(Ok(old.to_vec()))));
    assert!(
        matches!(&reasons[0], Stale::StampUnreadable(why) if why.contains("layout 1")),
        "{reasons:?}"
    );
}

#[test]
fn a_write_that_found_the_machine_changed_names_what_changed() {
    let f = nvidia_machine("600.20.03");
    let before = Current::read(&f);
    assert!(Current::read(&f).changes_since(&before).is_empty());
    f.touch(
        "/usr/lib/sysimage/rpm/rpmdb.sqlite-wal",
        5_000_000_000_000_000_000,
    );
    assert_eq!(
        Current::read(&f).changes_since(&before),
        vec![Stale::Packages]
    );
}

#[test]
fn a_stamp_survives_its_json_round_trip() {
    let f = nvidia_machine("600.20.03");
    let bytes = stamp_now(&f);
    let back: Stamp = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(back.schema, STAMP_SCHEMA);
    assert_eq!(back.kernel_module.as_deref(), Some("600.20.03"));
    assert_eq!(back.nvidia_ctk.version.as_deref(), Some("1.20.1"));
    assert!(back.packages_changed_ns.is_some());
    assert_eq!(back.device_nodes.len(), FakeFacts::NVIDIA_NODES.len());
}
