//! The plain sentences (D14, §1): no "container", "CDI" or "Podman" in a
//! headline, an item's sentence or a step's bullet, on any stated machine;
//! those names belong in the detail.

use lmgw_api_types::machine::MachineCheck;
use lmgw_machine::CmdOut;

use crate::support::machine_world::{sentences, World};

/// The words a newcomer never meets in a sentence.
const PLUMBING: [&str; 3] = ["container", "cdi", "podman"];

/// A package name the person may have to recognise is allowed in a step's
/// bullet (§4.5's own bullet names the retired package).
const PACKAGE_NAMES: [&str; 1] = ["golang-github-nvidia-container-toolkit"];

fn assert_plain(label: &str, c: &MachineCheck) {
    let mut all = sentences(c);
    if let Some(plan) = &c.plan {
        all.extend(plan.steps.iter().map(|s| s.sentence.clone()));
    }
    for s in all {
        let mut low = s.to_lowercase();
        for name in PACKAGE_NAMES {
            low = low.replace(name, "");
        }
        for word in PLUMBING {
            assert!(!low.contains(word), "{label}: '{word}' in \"{s}\"");
        }
        assert!(!s.is_empty(), "{label}: an empty sentence");
    }
}

#[tokio::test]
async fn no_sentence_names_the_plumbing() {
    let mut checks: Vec<(&str, MachineCheck)> = Vec::new();

    let w = World::nvidia().await;
    checks.push(("ready", w.check().await));
    w.remove_toolkit();
    checks.push(("no toolkit", w.check().await));

    let w = World::nvidia().await;
    *w.owner.lock().unwrap() = Some(
        "golang-github-nvidia-container-toolkit\t(none)\t1.17.4\t1.fc43\tx86_64\t(none)\t(none)\n"
            .into(),
    );
    checks.push(("retired", w.check().await));

    let w = World::nvidia().await;
    w.m.fail_with(Some(CmdOut::failure(1, "failed to initialize NVML\n")));
    let prefix = w.state.snapshot().settings.container_prefix.clone();
    let _ = w
        .state
        .cdi()
        .ensure_fresh(&prefix, std::time::Duration::from_secs(60))
        .await;
    checks.push(("generation failed", w.check().await));

    let w = World::nvidia_at("600.10.01").await;
    w.m.facts.set_driver_libraries("600.20.03");
    checks.push(("restart", w.check().await));

    let w = World::nvidia().await;
    w.m.facts.remove("/proc/driver/nvidia/version");
    checks.push(("not running", w.check().await));
    w.m.facts.remove("/usr/lib64/libnvidia-ml.so.1");
    w.m.facts.program("/usr/bin/rpm", |args| {
        Ok(CmdOut::failure(
            1,
            format!("package {} is not installed\n", args.last().unwrap()),
        ))
    });
    checks.push(("no driver", w.check().await));

    let w = World::nvidia().await;
    w.m.facts.remove("/usr/lib64/libnvidia-ml.so.1");
    checks.push(("no libraries", w.check().await));

    let w = World::nvidia().await;
    w.m.facts.remove("/sys/bus/pci/devices");
    checks.push(("no card", w.check().await));

    let w = World::nvidia().await;
    *w.podman.missing.lock().unwrap() = true;
    checks.push(("no podman", w.check().await));
    *w.podman.missing.lock().unwrap() = false;
    *w.podman.info_fails.lock().unwrap() = Some("Error: storage is locked\n".into());
    checks.push(("podman broken", w.check().await));
    *w.podman.info_fails.lock().unwrap() = None;
    *w.podman.old.lock().unwrap() = true;
    checks.push(("podman old", w.check().await));

    let w = World::nvidia().await;
    *w.nvml.lock().unwrap() = Some(lmgw_core::machine::probe::NvmlAnswer::Failed {
        kind: "other".into(),
        error: "Unknown Error".into(),
    });
    checks.push(("nvml failed", w.check().await));

    // The review round's states: a write that ran out of the load timeout,
    // a folder that is not private, render nodes on an NVIDIA card.
    let w = World::nvidia().await;
    w.m.hang(std::time::Duration::from_secs(2));
    let prefix = w.state.snapshot().settings.container_prefix.clone();
    let _ = w
        .state
        .cdi()
        .ensure_fresh(&prefix, std::time::Duration::from_millis(200))
        .await;
    checks.push(("write timed out", w.check().await));

    let w = World::nvidia().await;
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = w.m.runtime_dir.path().join("lmgw-machine");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    let _ = w
        .state
        .cdi()
        .ensure_fresh(&prefix, std::time::Duration::from_secs(60))
        .await;
    checks.push(("folder not private", w.check().await));

    let w = World::nvidia().await;
    w.settings(|s| s.router.extra_run_args = vec!["--device".into(), "/dev/dri".into()])
        .await;
    checks.push(("render nodes", w.check().await));

    for (label, c) in &checks {
        assert_plain(label, c);
    }
    // And the detail does name them where it explains.
    let (_, ready) = &checks[0];
    assert!(ready
        .items
        .iter()
        .flat_map(|i| &i.detail)
        .any(|l| l.contains("podman")));
}
