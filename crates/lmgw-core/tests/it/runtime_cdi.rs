//! lmgw's own CDI spec (machine-check design §3.6, §6 "lmgw's own spec" and
//! "The podman option"; WP3): written once and then fresh, written again once
//! for each reason it went stale, never while the driver waits for a restart,
//! one generation for two starts at once, `nvidia-modprobe` first when the
//! uvm node is missing, a failed generation failing the GPU start, a hung one
//! stopped at the load timeout, dirs that are not private refused, Podman's
//! default dirs when there is no toolkit or no option, and where the spec
//! lives per instance.
//!
//! The machine is `support/cdi_machine.rs`'s: nothing of the computer the
//! suite runs on is read, and the spec lands in a temp `XDG_RUNTIME_DIR`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::erased::Erased;
use lmgw_core::runtime::cdi::{CdiError, DefaultDirs, Ensured, SPEC_DIR_OPTION};
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner};
use lmgw_machine::cdi::own::{Blocked, Stale, Stamp, NVIDIA_MODPROBE, NVIDIA_UVM};
use lmgw_machine::CmdOut;

use crate::support::cdi_machine::{add_paths_of, CdiMachine, HelpPodman, SPEC};

mod podman_option;
mod podman_sites;
mod refusal;

/// A load timeout no test reaches.
const BUDGET: Duration = Duration::from_secs(60);

fn data_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("lmgw-cdi-data-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn gpu_args() -> Vec<String> {
    args(&[
        "--device",
        "nvidia.com/gpu=all",
        "--security-opt",
        "label=disable",
    ])
}

#[tokio::test]
async fn the_first_ensure_writes_the_spec_and_its_stamp_and_the_next_finds_it_fresh() {
    let m = CdiMachine::new();
    let data = data_dir();
    let own = m.own_spec(data.path());
    let paths = own.paths("lmgw").unwrap();
    assert_eq!(paths.spec_dir, m.spec_dir("lmgw"));

    let first = own.ensure_fresh("lmgw", BUDGET).await.unwrap();
    assert_eq!(first, Ensured::Written(vec![Stale::NoSpec, Stale::NoStamp]));
    assert_eq!(std::fs::read_to_string(&paths.spec).unwrap(), SPEC);
    // The spec dir holds the spec and nothing else: Podman reads every
    // `*.yaml` and `*.json` there. The stamp is beside it, the staging dir
    // is gone.
    assert_eq!(names_in(&paths.spec_dir), vec!["nvidia.json"]);
    assert_eq!(names_in(&paths.root), vec!["cdi", "nvidia.stamp.json"]);
    let stamp: Stamp = serde_json::from_slice(&std::fs::read(&paths.stamp).unwrap()).unwrap();
    assert_eq!(stamp.kernel_module.as_deref(), Some("600.20.03"));
    assert_eq!(stamp.userspace.as_deref(), Some("600.20.03"));
    assert!(stamp.boot_id.is_some());
    assert!(stamp.device_nodes.contains(&"nvidia-uvm".to_string()));
    assert_eq!(stamp.nvidia_ctk.version.as_deref(), Some("1.20.1"));
    assert!(stamp.packages_changed_ns.is_some());
    {
        use std::os::unix::fs::PermissionsExt;
        for dir in [
            &paths.root.parent().unwrap().to_path_buf(),
            &paths.root,
            &paths.spec_dir,
        ] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
    }

    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Fresh
    );
    assert_eq!(m.generations(), 1);
    let generate_calls = m.generate_calls();
    assert_eq!(generate_calls.len(), 1);
    let call = &generate_calls[0];
    assert_eq!(call[..3], ["cdi", "generate", "--format=json"]);
    assert!(
        call[3].starts_with("--output=") && !call[3].contains("/cdi/"),
        "staged outside the spec dir: {call:?}"
    );
    // The hook it names is the toolkit's own, not the first one on a PATH.
    assert_eq!(call[4], "--nvidia-cdi-hook-path=/usr/bin/nvidia-cdi-hook");
    assert!(own.last().unwrap().outcome.is_ok());
}

#[tokio::test]
async fn an_egl_wayland_update_that_deletes_a_named_library_writes_it_again_once() {
    // An egl-wayland update, 1.1.23 → 1.1.24: the driver version untouched,
    // the old library deleted.
    let m = CdiMachine::new();
    let old = SPEC.replace(
        "libnvidia-egl-wayland.so.1.1.24",
        "libnvidia-egl-wayland.so.1.1.23",
    );
    add_paths_of(&m.facts, &old);
    m.generates(&old);
    let data = data_dir();
    let own = m.own_spec(data.path());
    own.ensure_fresh("lmgw", BUDGET).await.unwrap();
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Fresh
    );

    m.facts.remove("/usr/lib64/libnvidia-egl-wayland.so.1.1.23");
    m.generates(SPEC);
    let again = own.ensure_fresh("lmgw", BUDGET).await.unwrap();
    let Ensured::Written(why) = &again else {
        panic!("{again:?}")
    };
    assert_eq!(why.len(), 1, "{why:?}");
    let Stale::PathsGone(gone) = &why[0] else {
        panic!("{why:?}")
    };
    assert_eq!(
        gone[0].0,
        PathBuf::from("/usr/lib64/libnvidia-egl-wayland.so.1.1.23")
    );
    assert!(std::fs::read_to_string(own.paths("lmgw").unwrap().spec)
        .unwrap()
        .contains("libnvidia-egl-wayland.so.1.1.24"));
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Fresh
    );
    assert_eq!(m.generations(), 2);
}

#[tokio::test]
async fn a_package_transaction_or_a_new_nvidia_ctk_writes_it_again_once_each() {
    let m = CdiMachine::new();
    let data = data_dir();
    let own = m.own_spec(data.path());
    own.ensure_fresh("lmgw", BUDGET).await.unwrap();

    // A transaction after the spec was written (2027-01-15).
    m.facts.touch(
        "/usr/lib/sysimage/rpm/rpmdb.sqlite-wal",
        1_800_000_000_000_000_000,
    );
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Written(vec![Stale::Packages])
    );
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Fresh
    );

    m.facts.replace_inode("/usr/bin/nvidia-ctk");
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Written(vec![Stale::Toolkit {
            was: Some("1.20.1".into())
        }])
    );
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Fresh
    );
    assert_eq!(m.generations(), 3);
}

#[tokio::test]
async fn across_a_driver_update_nothing_is_written_until_the_restart() {
    let m = CdiMachine::at("600.10.01");
    let data = data_dir();
    let own = m.own_spec(data.path());
    own.ensure_fresh("lmgw", BUDGET).await.unwrap();
    let spec = own.paths("lmgw").unwrap().spec;
    let before = std::fs::read(&spec).unwrap();
    let stamp_before = std::fs::read(own.paths("lmgw").unwrap().stamp).unwrap();

    // The update: libraries on disk at 600.20.03, the old module running.
    m.facts.set_driver_libraries("600.20.03");
    let err = own.ensure_fresh("lmgw", BUDGET).await.unwrap_err();
    assert_eq!(
        err,
        CdiError::Blocked(Blocked::Restart {
            kernel: "600.10.01".into(),
            userspace: "600.20.03".into()
        })
    );
    assert!(err.to_string().starts_with("Restart the computer"), "{err}");
    assert_eq!(m.generations(), 1, "nothing generated while they differ");
    assert_eq!(
        std::fs::read(&spec).unwrap(),
        before,
        "the last spec is kept"
    );
    assert_eq!(
        std::fs::read(own.paths("lmgw").unwrap().stamp).unwrap(),
        stamp_before
    );

    // The restart loads the new module: written again, for the new driver.
    m.facts.set_kernel_module("600.20.03");
    let Ensured::Written(why) = own.ensure_fresh("lmgw", BUDGET).await.unwrap() else {
        panic!("not written after the restart")
    };
    assert!(why.contains(&Stale::KernelModule {
        was: Some("600.10.01".into()),
        now: Some("600.20.03".into())
    }));
    assert!(why.contains(&Stale::Userspace {
        was: Some("600.10.01".into()),
        now: Some("600.20.03".into())
    }));
    assert_eq!(m.generations(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_starts_at_once_share_one_generation() {
    let m = CdiMachine::new();
    m.slow(Duration::from_millis(200));
    let data = data_dir();
    let own = m.own_spec(data.path());
    let (a, b) = tokio::join!(
        own.ensure_fresh("lmgw", BUDGET),
        own.ensure_fresh("lmgw", BUDGET)
    );
    let mut outcomes = [a.unwrap(), b.unwrap()];
    outcomes.sort_by_key(|o| matches!(o, Ensured::Fresh));
    assert!(matches!(outcomes[0], Ensured::Written(_)), "{outcomes:?}");
    assert_eq!(
        outcomes[1],
        Ensured::Fresh,
        "the second waited and found it fresh"
    );
    assert_eq!(m.generations(), 1);
}

#[tokio::test]
async fn a_missing_uvm_node_is_created_with_nvidia_modprobe_first() {
    let m = CdiMachine::new();
    m.facts.remove(NVIDIA_UVM);
    let weak = Arc::downgrade(&m.facts);
    m.facts.program(NVIDIA_MODPROBE, move |args| {
        assert_eq!(args, ["-u", "-c=0"]);
        if let Some(f) = weak.upgrade() {
            f.file(NVIDIA_UVM, b"");
        }
        Ok(CmdOut::success(""))
    });
    let data = data_dir();
    let own = m.own_spec(data.path());
    own.ensure_fresh("lmgw", BUDGET).await.unwrap();
    let order: Vec<String> = m
        .facts
        .calls()
        .into_iter()
        .filter(|c| {
            c.program != Path::new("/usr/bin/nvidia-ctk")
                || (c.args[0] == "cdi" && !c.args.contains(&"--help".to_string()))
        })
        .map(|c| c.program.display().to_string())
        .collect();
    assert_eq!(order, vec![NVIDIA_MODPROBE, "/usr/bin/nvidia-ctk"]);
    // The stamp records the node set after the uvm node was made: the next
    // judgement finds it fresh.
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Fresh
    );
}

#[tokio::test]
async fn a_failed_generation_is_the_error_and_leaves_nothing_half_written() {
    let m = CdiMachine::new();
    m.fail_with(Some(CmdOut::failure(
        1,
        "time=\"…\" level=info msg=\"Selecting /dev/nvidia0 as /dev/nvidia0\"\n\
         time=\"…\" level=error msg=\"failed to generate CDI spec: failed to initialize NVML: \
         Driver/library version mismatch\"\n",
    )));
    let data = data_dir();
    let own = m.own_spec(data.path());
    let err = own.ensure_fresh("lmgw", BUDGET).await.unwrap_err();
    let CdiError::Generate { status, said } = &err else {
        panic!("{err:?}")
    };
    assert_eq!(status, "1");
    assert_eq!(said.len(), 1);
    assert!(said[0].contains("version mismatch"));
    let text = err.to_string();
    assert!(
        text.starts_with("lmgw could not write the description of the graphics card"),
        "{text}"
    );
    let paths = own.paths("lmgw").unwrap();
    assert!(!paths.spec.exists());
    assert!(!paths.stamp.exists());
    assert_eq!(
        names_in(&paths.root),
        vec!["cdi"],
        "the staging dir is gone"
    );
}

/// D26: without `nvidia-ctk` there is nothing to write the spec with, so
/// lmgw's containers use Podman's default spec dirs, as before ruling 4, and
/// the spec says why for the machine check.
#[tokio::test]
async fn without_the_toolkit_podmans_default_dirs_are_used_and_the_reason_is_kept() {
    let m = CdiMachine::new();
    m.facts.uninstall("/usr/bin/nvidia-ctk");
    let data = data_dir();
    let own = m.own_spec(data.path());
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::DefaultDirs(DefaultDirs::NoToolkit)
    );
    assert_eq!(own.default_dirs(), Some(DefaultDirs::NoToolkit));
    assert!(DefaultDirs::NoToolkit
        .to_string()
        .contains("container toolkit is not installed"));
    assert!(own
        .ready_globals("lmgw", &gpu_args(), BUDGET)
        .await
        .unwrap()
        .is_empty());
    assert!(own.globals("lmgw", &gpu_args()).is_empty());
    assert!(matches!(
        own.last().unwrap().outcome,
        Ok(Ensured::DefaultDirs(DefaultDirs::NoToolkit))
    ));
    assert_eq!(m.generations(), 0);
    // Installed again: lmgw's own spec from the next start on.
    m.install_toolkit();
    assert_eq!(own.default_dirs(), None);
    assert!(matches!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Written(_)
    ));
}

/// A Podman whose `--help` lists no `--cdi-spec-dir` would refuse the
/// option on every GPU start: lmgw asks once and uses Podman's default dirs
/// (D26).
#[tokio::test]
async fn a_podman_without_the_option_is_asked_once_and_gets_none() {
    let m = CdiMachine::new();
    let data = data_dir();
    let podman = Arc::new(HelpPodman {
        old: true,
        ..Default::default()
    });
    let own = m.own_spec_with(data.path(), podman.clone());
    for _ in 0..2 {
        assert!(own
            .ready_globals("lmgw", &gpu_args(), BUDGET)
            .await
            .unwrap()
            .is_empty());
    }
    assert_eq!(podman.asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(own.default_dirs(), Some(DefaultDirs::PodmanWithoutOption));
    assert!(own.globals("lmgw", &gpu_args()).is_empty());
    assert_eq!(m.generations(), 0, "nothing to write a spec for");
    // CPU args never ask.
    let unasked = Arc::new(HelpPodman::default());
    let cpu = m.own_spec_with(data.path(), unasked.clone());
    let none = cpu
        .ready_globals("lmgw", &args(&["--cpus", "4"]), BUDGET)
        .await
        .unwrap();
    assert!(none.is_empty());
    assert_eq!(unasked.asked.load(std::sync::atomic::Ordering::SeqCst), 0);

    // A current Podman: asked once, and the option passed.
    let current = Arc::new(HelpPodman::default());
    let own = m.own_spec_with(data.path(), current.clone());
    let globals = own
        .ready_globals("lmgw", &gpu_args(), BUDGET)
        .await
        .unwrap();
    assert_eq!(globals[0], SPEC_DIR_OPTION);
    let _ = own
        .ready_globals("lmgw", &gpu_args(), BUDGET)
        .await
        .unwrap();
    assert_eq!(current.asked.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// D26: a driver library whose version cannot be read does not keep
/// the spec from being written — nvidia-ctk's own NVML start says when the
/// halves really differ.
#[tokio::test]
async fn an_unversioned_driver_library_still_gets_a_spec() {
    let m = CdiMachine::new();
    m.facts
        .remove("/usr/lib64/libnvidia-ml.so.1")
        .file("/usr/lib64/libnvidia-ml.so.1", b"\x7fELF");
    let data = data_dir();
    let own = m.own_spec(data.path());
    assert!(matches!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Written(_)
    ));
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Fresh
    );
}

/// An NVML that does not answer (a card off the bus) hangs nvidia-ctk: the
/// start's load timeout stops it, and the start says which program ran how
/// long and which setting bounds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hung_generation_is_stopped_at_the_load_timeout_and_says_so() {
    let m = CdiMachine::new();
    m.hang(Duration::from_secs(5));
    let data = data_dir();
    let own = m.own_spec(data.path());
    let started = std::time::Instant::now();
    let err = own
        .ensure_fresh("lmgw", Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    let CdiError::TimedOut { program, ran, .. } = &err else {
        panic!("{err:?}")
    };
    assert_eq!(program, "nvidia-ctk cdi generate");
    assert!(*ran <= Duration::from_secs(2), "{ran:?}");
    let text = err.to_string();
    assert!(
        text.starts_with("`nvidia-ctk cdi generate` ran for")
            && text.contains("Settings → GPU → Admission → Load timeout"),
        "{text}"
    );
    assert!(!own.paths("lmgw").unwrap().spec.exists());
    assert!(matches!(
        own.last().unwrap().outcome,
        Err(CdiError::TimedOut { .. })
    ));
}

/// The wait for another write is bounded by the same load timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wait_for_another_write_is_bounded_too() {
    let m = CdiMachine::new();
    m.slow(Duration::from_millis(1500));
    let data = data_dir();
    let own = m.own_spec(data.path());
    let first = own.ensure_fresh("lmgw", BUDGET);
    let second = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        own.ensure_fresh("lmgw", Duration::from_millis(200)).await
    };
    let (first, second) = tokio::join!(first, second);
    assert!(matches!(first.unwrap(), Ensured::Written(_)));
    let err = second.unwrap_err();
    assert_eq!(
        err,
        CdiError::Waited {
            waited: Duration::from_millis(200)
        }
    );
    assert!(err.to_string().contains("Load timeout"), "{err}");
}

/// The dirs lmgw keeps the spec in are its own: one another user could write
/// into is refused, and nothing is written or changed there. (One others can
/// only read is closed to them and used.)
#[tokio::test]
async fn a_spec_dir_others_may_enter_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let m = CdiMachine::new();
    let data = data_dir();
    let own = m.own_spec(data.path());
    let machine_dir = m.runtime_dir.path().join("lmgw-machine");
    std::fs::create_dir(&machine_dir).unwrap();
    std::fs::set_permissions(&machine_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let err = own.ensure_fresh("lmgw", BUDGET).await.unwrap_err();
    let CdiError::NotPrivate { dir, why } = &err else {
        panic!("{err:?}")
    };
    assert_eq!(dir, &machine_dir);
    assert!(why.contains("0777"), "{why}");
    assert!(err.to_string().contains("so lmgw does not use it"), "{err}");
    assert!(names_in(&machine_dir).is_empty(), "nothing made inside");
    assert_eq!(m.generations(), 0);
}

/// Each change the spec has to follow writes it again once: a new
/// `/dev/nvidia*` node, a reboot, a file beside it in the spec dir (removed:
/// Podman would read it too), and the staging dir a killed lmgw left.
#[tokio::test]
async fn new_nodes_a_reboot_and_stray_files_write_it_again_once_each() {
    let m = CdiMachine::new();
    let data = data_dir();
    let own = m.own_spec(data.path());
    own.ensure_fresh("lmgw", BUDGET).await.unwrap();
    let paths = own.paths("lmgw").unwrap();

    m.facts.file("/dev/nvidia1", b"");
    let Ensured::Written(why) = own.ensure_fresh("lmgw", BUDGET).await.unwrap() else {
        panic!("not written for a new node")
    };
    assert!(matches!(why[..], [Stale::DeviceNodes { .. }]), "{why:?}");

    m.facts.set_boot_id("0f6c1e9a-5b7d-4e2a-9c3f-0000000000ff");
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Written(vec![Stale::Reboot])
    );

    // The YAML spec an earlier build wrote, and a stray directory.
    std::fs::write(paths.spec_dir.join("nvidia.yaml"), "kind: nvidia.com/gpu\n").unwrap();
    std::fs::create_dir(paths.spec_dir.join("sub")).unwrap();
    // A staging dir of an lmgw process that is gone (no pid is this high).
    let stray = paths.root.join(format!(".generating-{}", 1 << 23));
    std::fs::create_dir(&stray).unwrap();
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Written(vec![Stale::ExtraFiles(vec![
            "nvidia.yaml".into(),
            "sub".into()
        ])])
    );
    assert_eq!(names_in(&paths.spec_dir), vec!["nvidia.json"]);
    assert!(!stray.exists());
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Fresh
    );
    assert_eq!(m.generations(), 4);
}

/// A package transaction that lands while nvidia-ctk runs: what it wrote may
/// describe neither machine, so it is not used. The write runs once more from
/// a fresh reading; a machine that changes under that one too is the start's
/// answer, and the next start writes it.
#[tokio::test]
async fn a_machine_that_changed_during_the_write_keeps_the_new_spec_out() {
    let m = CdiMachine::new();
    let data = data_dir();
    let own = m.own_spec(data.path());
    let tick = std::sync::atomic::AtomicU64::new(1_900_000_000_000_000_000);
    m.while_generating(move |f| {
        f.touch(
            "/usr/lib/sysimage/rpm/rpmdb.sqlite-wal",
            i128::from(tick.fetch_add(1_000_000_000, std::sync::atomic::Ordering::SeqCst)),
        );
    });
    let err = own.ensure_fresh("lmgw", BUDGET).await.unwrap_err();
    assert_eq!(err, CdiError::Changed(vec![Stale::Packages]));
    assert!(
        err.to_string().contains("the next start writes it again"),
        "{err}"
    );
    assert!(!own.paths("lmgw").unwrap().spec.exists());
    assert_eq!(m.generate_calls().len(), 2, "once more, not forever");
}

/// A change during the first write only (a second card's node made by NVML on
/// a machine just booted) costs one more write, not the start.
#[tokio::test]
async fn a_change_during_the_first_write_only_writes_it_once_more() {
    let m = CdiMachine::new();
    let data = data_dir();
    let own = m.own_spec(data.path());
    let once = std::sync::atomic::AtomicBool::new(true);
    m.while_generating(move |f| {
        if once.swap(false, std::sync::atomic::Ordering::SeqCst) {
            f.touch(
                "/usr/lib/sysimage/rpm/rpmdb.sqlite-wal",
                1_900_000_000_000_000_000,
            );
        }
    });
    let done = own.ensure_fresh("lmgw", BUDGET).await.unwrap();
    assert!(matches!(done, Ensured::Written(_)), "{done:?}");
    assert!(own.paths("lmgw").unwrap().spec.exists());
    assert_eq!(m.generate_calls().len(), 2);
    assert_eq!(
        own.ensure_fresh("lmgw", BUDGET).await.unwrap(),
        Ensured::Fresh
    );
}

/// A toolkit without the hook flag (before 1.17) is asked without it.
#[tokio::test]
async fn a_toolkit_without_the_hook_flag_is_asked_without_it() {
    let m = CdiMachine::new();
    m.old_toolkit();
    let data = data_dir();
    let own = m.own_spec(data.path());
    own.ensure_fresh("lmgw", BUDGET).await.unwrap();
    let call = &m.generate_calls()[0];
    assert!(
        !call.iter().any(|a| a.starts_with("--nvidia-cdi-hook-path")),
        "a toolkit without the flag is not given it: {call:?}"
    );
}

#[test]
fn each_instance_has_its_own_dir_and_the_data_dir_is_the_fallback() {
    let m = CdiMachine::new();
    let data = data_dir();
    let own = m.own_spec(data.path());
    let prod = own.paths("lmgw").unwrap();
    let dev = own.paths("lmgw-dev").unwrap();
    assert_ne!(
        prod.spec_dir, dev.spec_dir,
        "a dev instance never writes production's"
    );
    assert_eq!(dev.spec_dir, m.spec_dir("lmgw-dev"));
    // Not under the agents' runs root, whose sweep removes what is not a run.
    assert!(!prod.root.starts_with(m.runtime_dir.path().join("lmgw")));

    m.facts.unset_env("XDG_RUNTIME_DIR");
    assert_eq!(
        own.paths("lmgw").unwrap().spec_dir,
        data.path().join("machine").join("cdi")
    );
    // A relative one names nothing in particular: the data dir as well.
    m.facts.set_env("XDG_RUNTIME_DIR", "run/user/1000");
    assert_eq!(
        own.paths("lmgw").unwrap().spec_dir,
        data.path().join("machine").join("cdi")
    );
}

#[test]
fn the_option_names_lmgws_dir_alone_for_nvidia_and_the_system_dirs_first_for_another_kind() {
    let m = CdiMachine::new();
    let data = data_dir();
    let own = m.own_spec(data.path());
    let dir = m.spec_dir("lmgw").display().to_string();

    assert_eq!(
        own.globals("lmgw", &gpu_args()),
        vec![SPEC_DIR_OPTION.to_string(), dir.clone()]
    );
    assert_eq!(
        own.globals("lmgw", &args(&["--gpus", "all"])),
        vec![SPEC_DIR_OPTION.to_string(), dir.clone()]
    );
    for cpu in [
        args(&["-e", "CUDA_VISIBLE_DEVICES=", "--cpus", "8"]),
        args(&["--device", "/dev/dri", "--device", "/dev/kfd"]),
        vec![],
    ] {
        assert!(own.globals("lmgw", &cpu).is_empty(), "{cpu:?}");
    }

    let mut other = gpu_args();
    other.extend(args(&["--device", "vendor.example/fpga=0"]));
    assert_eq!(
        own.globals("lmgw", &other),
        args(&[
            "--cdi-spec-dir",
            "/etc/cdi",
            "--cdi-spec-dir",
            "/var/run/cdi",
            "--cdi-spec-dir",
            &dir
        ])
    );
    // containers.conf's dirs, read the way Podman reads them.
    m.facts.file(
        "/etc/containers/containers.conf",
        "[engine]\ncdi_spec_dirs = [\"/opt/cdi\"]\n",
    );
    assert_eq!(
        own.globals("lmgw", &args(&["--device", "vendor.example/fpga=0"])),
        args(&["--cdi-spec-dir", "/opt/cdi", "--cdi-spec-dir", &dir])
    );
}

#[test]
fn an_unmanaged_spec_gives_no_option() {
    let own = lmgw_core::runtime::cdi::OwnSpec::unmanaged("a test");
    assert!(own.globals("lmgw", &gpu_args()).is_empty());
    assert_eq!(own.why_unmanaged(), Some("a test"));
}

// ---------------------------------------------------------------------------
// A GPU start whose spec cannot be written
// ---------------------------------------------------------------------------

/// A podman that records every call and answers each with success.
#[derive(Default)]
pub(crate) struct Recorder {
    pub calls: Mutex<Vec<Vec<String>>>,
    /// What a `run` prints (a throwaway's output).
    pub run_stdout: Mutex<String>,
}

impl Recorder {
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    /// Every call that runs a container, whatever global options come first.
    pub fn runs(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| c.iter().any(|a| a == "run"))
            .collect()
    }
}

impl CommandRunner for Recorder {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        Box::pin(async move {
            assert_eq!(program, "podman");
            self.calls.lock().unwrap().push(args.to_vec());
            let stdout = if args.iter().any(|a| a == "run") {
                self.run_stdout.lock().unwrap().clone()
            } else {
                "c0ffee\n".to_string()
            };
            Ok(CmdOutput {
                status: 0,
                stdout,
                stderr: String::new(),
            })
        })
    }
}

#[tokio::test]
async fn a_gpu_start_whose_spec_cannot_be_written_fails_with_why_and_runs_nothing() {
    use lmgw_core::runtime::registry::{AcquireSpec, Registry, RuntimeError};

    let m = CdiMachine::new();
    m.fail_with(Some(CmdOut::failure(
        1,
        "level=error msg=\"failed to initialize NVML: Unknown Error\"\n",
    )));
    let data = data_dir();
    let podman = Arc::new(Recorder::default());
    let reg = Arc::new(Registry::with_ports(
        podman.clone(),
        reqwest::Client::new(),
        Arc::new(|| Ok(1)),
    ));
    reg.set_cdi(m.own_spec(data.path()));
    let rt = podman_option::chat_runtime("qwen", gpu_args());
    let err = reg
        .acquire(&AcquireSpec {
            runtime: &rt,
            container_prefix: "lmgw",
            models_dir: "/srv/models",
            data_dir: data.path(),
            may_write_models_dir: true,
            load_timeout: Duration::from_millis(500),
            stop_timeout: Duration::from_millis(500),
        })
        .await
        .unwrap_err();
    let RuntimeError::Start { message, .. } = &err else {
        panic!("{err:?}")
    };
    assert!(
        message.contains("could not write the description of the graphics card")
            && message.contains("Unknown Error"),
        "{message}"
    );
    assert!(podman.runs().is_empty(), "{:?}", podman.calls());
}
