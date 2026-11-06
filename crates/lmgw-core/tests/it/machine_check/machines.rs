//! The verdict on each kind of machine (§1.1–§1.7).

use lmgw_api_types::machine::{
    ItemState, MachineCause, MachineItemId, MachineVerdict, PlanHow, PlanStepKind,
};
use lmgw_machine::CmdOut;

use crate::support::machine_world::World;

fn item(
    c: &lmgw_api_types::machine::MachineCheck,
    id: MachineItemId,
) -> &lmgw_api_types::machine::MachineItem {
    c.items.iter().find(|i| i.id == id).unwrap()
}

/// A working RTX 3090 with NVIDIA's toolkit: ready, the card named with its
/// memory, NVML asked once in a process of its own, no plan.
#[tokio::test]
async fn a_working_nvidia_machine_is_ready() {
    let w = World::nvidia().await;
    w.podman
        .images
        .lock()
        .unwrap()
        .insert("ghcr.io/ggml-org/llama.cpp:server-cuda".into());
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::Ready, "{c:#?}");
    assert_eq!(c.headline, "This computer is ready for local models.");
    let ids: Vec<MachineItemId> = c.items.iter().map(|i| i.id).collect();
    assert_eq!(
        ids,
        [
            MachineItemId::GraphicsCard,
            MachineItemId::CardForModels,
            MachineItemId::RunningModels,
            MachineItemId::SettingsFit,
            MachineItemId::Room,
            MachineItemId::Download
        ]
    );
    let card = item(&c, MachineItemId::GraphicsCard);
    assert_eq!(
        card.sentence,
        "Graphics card · NVIDIA GeForce RTX 3090 · 22.5 GiB"
    );
    assert!(
        card.detail
            .iter()
            .any(|l| l.contains("kernel module 600.20.03 (open)")),
        "{:?}",
        card.detail
    );
    assert!(card
        .detail
        .iter()
        .any(|l| l.contains("NVML in a separate process")));
    assert_eq!(item(&c, MachineItemId::CardForModels).state, ItemState::Ok);
    assert_eq!(
        item(&c, MachineItemId::RunningModels).sentence,
        "lmgw can run models on this computer."
    );
    assert!(item(&c, MachineItemId::RunningModels).detail[0]
        .starts_with("podman version 5.8.9, rootless, crun, cgroup v2"));
    assert_eq!(item(&c, MachineItemId::SettingsFit).state, ItemState::Ok);
    assert_eq!(
        item(&c, MachineItemId::Room).sentence,
        "Room for models · 466 GiB free."
    );
    assert!(c.plan.is_none());
    assert_eq!(w.probes(), 1);
    // The probe is lmgw's own image, started for it.
    let calls = w.m.facts.calls_of("/proc/self/exe");
    assert_eq!(calls, [["--machine-probe", "nvml"]]);
}

/// No toolkit: one piece missing, and the plan of §3.2 — as instructions
/// while no helper is installed (this build has none), behind the button
/// once it is.
#[tokio::test]
async fn no_toolkit_offers_the_plan() {
    let w = World::nvidia().await;
    w.remove_toolkit();
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps, "{c:#?}");
    let toolkit = item(&c, MachineItemId::CardForModels);
    assert_eq!(toolkit.state, ItemState::Problem);
    assert_eq!(toolkit.cause, Some(MachineCause::ToolkitMissing));
    assert_eq!(
        c.headline,
        "One piece is missing so models can use the graphics card. Here's how to add it."
    );
    let plan = c.plan.as_ref().unwrap();
    let kinds: Vec<PlanStepKind> = plan.steps.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        [
            PlanStepKind::S1AddRepo,
            PlanStepKind::S2AddKey,
            PlanStepKind::S4Install
        ]
    );
    let PlanHow::Instructions { why } = &plan.how else {
        panic!("{:?}", plan.how)
    };
    assert!(
        why.starts_with("This version of lmgw can't do this by itself yet"),
        "{why}"
    );
    assert!(
        toolkit
            .detail
            .iter()
            .any(|l| l.contains("no setup helper installed")),
        "the helper's why is a detail line: {:?}",
        toolkit.detail
    );
    // The hash is lmgw-machine's over the steps as root runs them.
    let inputs = lmgw_machine::plan::Inputs::read(&*w.m.facts, lmgw_machine::plan::PROVIDERS);
    let expected = lmgw_machine::plan::plan(
        &inputs,
        lmgw_machine::plan::Want::default(),
        lmgw_machine::plan::PROVIDERS,
    );
    assert_eq!(plan.hash, expected.hash());
    assert_eq!(plan.steps[2].commands, expected.steps[2].commands());
    assert!(plan.steps[0]
        .writes_file
        .as_ref()
        .unwrap()
        .content
        .starts_with("# Written by lmgw."));
    assert!(plan.steps[2].terminal[0].starts_with("sudo dnf5 install -y "));
    // The fold's facts beside the commands (§4.5): each file's mode, the
    // key's fingerprint computed from the text it writes, and what NVIDIA's
    // package changes by itself; none of them in the hash.
    let repo = plan.steps[0].writes_file.as_ref().unwrap();
    let key = plan.steps[1].writes_file.as_ref().unwrap();
    assert_eq!((repo.mode, key.mode), (0o644, 0o644));
    assert_eq!(repo.key_fingerprint, None);
    assert_eq!(
        key.key_fingerprint.as_deref(),
        Some("C95B 321B 61E8 8C18 09C4 F759 DDCA E044 F796 ECB0")
    );
    assert_eq!(plan.steps[0].package_scripts, None);
    assert_eq!(plan.steps[1].package_scripts, None);
    assert_eq!(
        plan.steps[2].package_scripts.as_deref(),
        Some(lmgw_machine::plan::scripts::INSTALL_SCRIPTS)
    );

    // With the helper installed: the button.
    w.m.facts
        .file_mode(lmgw_machine::plan::embedded::HELPER, b"\x7fELF", 0o755)
        .file_mode(
            lmgw_machine::plan::embedded::POLICY,
            b"<policyconfig/>",
            0o644,
        )
        .file_mode(lmgw_machine::plan::embedded::PKEXEC, b"\x7fELF", 0o4755);
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::NeedsFix);
    assert_eq!(c.plan.as_ref().unwrap().how, PlanHow::Helper);
    assert_eq!(
        c.headline,
        "One piece is missing so models can use the graphics card. lmgw can set it up."
    );
    // The sentences and sizes are not hashed: the same plan, the same hash.
    assert_eq!(c.plan.unwrap().hash, expected.hash());
}

/// The plan the check offers is root's even where the person's own
/// environment moves rpm's answers (D87): here a `~/.rpmmacros` whose
/// `%_keyringpath` hides NVIDIA's key from the user's `rpmkeys --list`. Read
/// with the user's environment the plan would import the key, and the
/// helper, which reads without it, would refuse that hash (exit 2) after
/// every password.
#[tokio::test]
async fn the_offered_plan_is_roots_whatever_the_users_environment_says() {
    let w = World::nvidia().await;
    w.remove_toolkit();
    let before = w.check().await.plan.unwrap();
    w.m.facts
        .program_in_users_env("/usr/bin/rpmkeys", |_| Ok(CmdOut::success("")));
    let c = w.check().await;
    let plan = c.plan.as_ref().unwrap();
    assert_eq!(
        plan.hash, before.hash,
        "the user's environment moved the plan"
    );
    let kinds: Vec<PlanStepKind> = plan.steps.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        [
            PlanStepKind::S1AddRepo,
            PlanStepKind::S2AddKey,
            PlanStepKind::S4Install
        ]
    );
    // Root trusts the key already: lmgw's key file, no import.
    assert!(plan.steps[1].commands.is_empty(), "{:?}", plan.steps[1]);
    // Root's own plans, as the helper computes them from its view, accept it.
    let root = lmgw_machine::Facts::root_env(&*w.m.facts).unwrap();
    let inputs = lmgw_machine::plan::Inputs::read(&*root, lmgw_machine::plan::PROVIDERS);
    assert!(
        lmgw_machine::plan::accepted_hashes(&inputs, lmgw_machine::plan::PROVIDERS)
            .contains(&plan.hash)
    );
}

/// lmgw running across a driver update (§6): ready at 600.10.01; the
/// libraries on disk move to 600.20.03 while the module stays — restart,
/// no plan, no NVML probe, no spec written; after the restart, ready, and
/// the boot run writes the spec.
#[tokio::test]
async fn a_driver_update_waits_for_the_restart() {
    let w = World::nvidia_at("600.10.01").await;
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::Ready, "{c:#?}");
    assert_eq!(w.probes(), 1);

    w.m.facts.set_driver_libraries("600.20.03");
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::NeedsRestart, "{c:#?}");
    assert_eq!(
        c.headline,
        "Restart the computer to finish the graphics driver update."
    );
    assert_eq!(
        item(&c, MachineItemId::GraphicsCard).cause,
        Some(MachineCause::DriverMismatch {
            kernel: "600.10.01".into(),
            userspace: "600.20.03".into()
        })
    );
    assert_eq!(
        item(&c, MachineItemId::CardForModels).state,
        ItemState::NotChecked
    );
    assert!(item(&c, MachineItemId::CardForModels)
        .sentence
        .contains("restart first"));
    assert!(c.plan.is_none());
    assert_eq!(w.probes(), 1, "no NVML probe while the halves differ");
    lmgw_core::machine::boot(&w.state).await;
    assert_eq!(
        w.m.generations(),
        0,
        "no spec written while a restart is pending"
    );

    // The restart: the new module, a new boot.
    w.m.facts.set_kernel_module("600.20.03");
    w.m.facts
        .set_boot_id("0f6c1e9a-5b7d-4e2a-9c3f-000000000002");
    lmgw_core::machine::boot(&w.state).await;
    let c = lmgw_core::machine::check(&w.state, false).await;
    assert_eq!(c.verdict, MachineVerdict::Ready, "{c:#?}");
    assert_eq!(w.m.generations(), 1, "the boot run wrote lmgw's spec");
    assert!(
        item(&c, MachineItemId::CardForModels)
            .detail
            .iter()
            .any(|l| l.contains("true of this machine now")),
        "{:?}",
        item(&c, MachineItemId::CardForModels).detail
    );
}

/// No NVIDIA or AMD card: cloud only, the rest not checked.
#[tokio::test]
async fn no_card_is_cloud_only() {
    let w = World::nvidia().await;
    w.m.facts.remove("/sys/bus/pci/devices");
    World::pci(&w.m.facts, "0000:00:02.0", 0x8086, 0xa780, Some("i915"));
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::CloudOnly);
    assert_eq!(
        c.headline,
        "No graphics card found. Cloud models work; local models need a graphics card."
    );
    assert!(matches!(
        &item(&c, MachineItemId::GraphicsCard).cause,
        Some(MachineCause::NoCard { others }) if others[0].contains("UHD Graphics 770")
    ));
    for id in [
        MachineItemId::CardForModels,
        MachineItemId::SettingsFit,
        MachineItemId::Download,
    ] {
        assert_eq!(item(&c, id).state, ItemState::NotChecked, "{id:?}");
    }
    assert_eq!(w.probes(), 0);
}

/// An NVIDIA card on `nouveau`, no NVIDIA driver at all: needs the driver,
/// with how Fedora installs it in the detail.
#[tokio::test]
async fn an_nvidia_card_on_nouveau_needs_the_driver() {
    let w = World::nvidia().await;
    let f = &w.m.facts;
    f.remove("/proc/driver/nvidia/version")
        .remove("/usr/lib64/libnvidia-ml.so.1");
    World::pci(
        f,
        crate::support::machine_world::NVIDIA_PCI,
        0x10de,
        0x2204,
        Some("nouveau"),
    );
    f.program("/usr/bin/rpm", |args| {
        Ok(CmdOut::failure(
            1,
            format!("package {} is not installed\n", args.last().unwrap()),
        ))
    });
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::NeedsDriver, "{c:#?}");
    assert_eq!(
        c.headline,
        "Your NVIDIA card needs NVIDIA's driver before it can run models."
    );
    let card = item(&c, MachineItemId::GraphicsCard);
    assert_eq!(
        card.cause,
        Some(MachineCause::NoNvidiaDriver {
            bound: Some("nouveau".into())
        })
    );
    assert!(card.detail.iter().any(|l| l.contains("akmod-nvidia")));
    assert_eq!(w.probes(), 0);
}

/// The driver installed and its module not running: restart; with Secure
/// Boot on and a module built before this boot, the enrolment sentence
/// instead (the rest of §1.1's Secure Boot cases are in driver_states.rs).
#[tokio::test]
async fn an_installed_driver_that_does_not_run() {
    let w = World::nvidia().await;
    w.m.facts.remove("/proc/driver/nvidia/version");
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::NeedsRestart, "{c:#?}");
    assert!(c
        .headline
        .starts_with("The graphics driver is installed but not running yet."));

    w.m.facts
        .dir("/sys/firmware/efi")
        .file(lmgw_machine::driver::SECURE_BOOT_VAR, [6u8, 0, 0, 0, 1]);
    crate::machine_check::driver_states::module_built_before_boot(&w);
    let c = w.check().await;
    assert_eq!(
        item(&c, MachineItemId::GraphicsCard).cause,
        Some(MachineCause::DriverNotRunning { secure_boot: true })
    );
    assert!(c.headline.contains("Secure Boot"), "{}", c.headline);
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
}

/// An AMD card with the NVIDIA defaults in the class settings: the settings
/// do not fit, the detail names each class's field by its label, and the
/// switch to AMD's settings is the fix (WP12, `settings_fit.rs`).
#[tokio::test]
async fn an_amd_card_with_nvidia_settings_does_not_fit() {
    let w = World::amd().await;
    let c = w.check().await;
    assert_eq!(
        item(&c, MachineItemId::GraphicsCard).sentence,
        "Graphics card · Advanced Micro Devices, Inc. [AMD/ATI] Phoenix1 · 14.9 GiB"
    );
    let fit = item(&c, MachineItemId::SettingsFit);
    assert_eq!(fit.cause, Some(MachineCause::SettingsForNvidia));
    assert!(
        fit.detail.iter().any(|l| l.starts_with(
            "Settings → Runtimes → Chat → Extra podman run args: --device nvidia.com/gpu=all"
        )),
        "{:?}",
        fit.detail
    );
    assert_eq!(c.verdict, MachineVerdict::NeedsFix);
    assert!(fit.action.is_some(), "{fit:#?}");
    assert_eq!(w.probes(), 0, "no NVML without an NVIDIA card");
}

/// No podman: one more program needed; the download is not checked.
#[tokio::test]
async fn no_podman_needs_one_more_program() {
    let w = World::nvidia().await;
    *w.podman.missing.lock().unwrap() = true;
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    assert_eq!(c.headline, "lmgw needs one more program to run models.");
    assert_eq!(
        item(&c, MachineItemId::RunningModels).cause,
        Some(MachineCause::PodmanMissing)
    );
    assert_eq!(
        item(&c, MachineItemId::Download).state,
        ItemState::NotChecked
    );

    // A Podman whose info fails names the account's missing ID ranges.
    *w.podman.missing.lock().unwrap() = false;
    *w.podman.info_fails.lock().unwrap() =
        Some("Error: cannot set up namespace using newuidmap\n".into());
    // The name comes from the user database (an SSSD account is not in
    // /etc/passwd), not from /etc/passwd.
    w.m.facts
        .set_user_name("someone")
        .file("/etc/subuid", "other:100000:65536\n")
        .file("/etc/subgid", "someone:100000:65536\n");
    let c = w.check().await;
    let running = item(&c, MachineItemId::RunningModels);
    assert_eq!(
        running.sentence,
        "lmgw can't start models on this computer yet."
    );
    assert!(
        running.detail.iter().any(|l| l.contains("/etc/subuid")),
        "{:?}",
        running.detail
    );
    assert!(!running.detail.iter().any(|l| l.contains("/etc/subgid")));

    // A Podman without --cdi-spec-dir.
    *w.podman.info_fails.lock().unwrap() = None;
    *w.podman.old.lock().unwrap() = true;
    let c = w.check().await;
    assert!(matches!(
        item(&c, MachineItemId::RunningModels).cause,
        Some(MachineCause::PodmanTooOld { .. })
    ));
}

/// lmgw's spec could not be written with NVIDIA's toolkit: the update (S4′)
/// is offered.
#[tokio::test]
async fn a_failed_generation_offers_the_update() {
    let w = World::nvidia().await;
    w.m.fail_with(Some(CmdOut::failure(
        1,
        "level=error msg=\"failed to initialize NVML\"\n",
    )));
    let prefix = w.state.snapshot().settings.container_prefix.clone();
    let e = w
        .state
        .cdi()
        .ensure_fresh(&prefix, std::time::Duration::from_secs(60))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("nvidia-ctk cdi generate"), "{e}");
    let c = w.check().await;
    let toolkit = item(&c, MachineItemId::CardForModels);
    assert!(
        matches!(&toolkit.cause, Some(MachineCause::SpecNotWritten { .. })),
        "{toolkit:#?}"
    );
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    let kinds: Vec<PlanStepKind> = c.plan.unwrap().steps.iter().map(|s| s.kind).collect();
    assert!(kinds.contains(&PlanStepKind::S4Update), "{kinds:?}");
}

/// Fedora's retired package that still works: ready, with the switch
/// offered next to it.
#[tokio::test]
async fn the_retired_package_is_ready_with_the_switch_offered() {
    let w = World::nvidia().await;
    *w.owner.lock().unwrap() = Some(
        "golang-github-nvidia-container-toolkit\t(none)\t1.17.4\t1.fc43\tx86_64\tRSA/SHA256, Mon Jan 12 2026, Key ID 829b606631645531\t(none)\n".into(),
    );
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::Ready, "{c:#?}");
    let toolkit = item(&c, MachineItemId::CardForModels);
    assert_eq!(toolkit.state, ItemState::Info);
    let plan = c.plan.unwrap();
    let swap = plan
        .steps
        .iter()
        .find(|s| s.kind == PlanStepKind::S4Swap)
        .unwrap();
    assert_eq!(
        swap.sentence,
        "Replace the piece Fedora stopped updating (golang-github-nvidia-container-toolkit 1.17.4) \
         with NVIDIA's current one"
    );
}

/// NVIDIA's software source that dnf has not loaded within its own expiry
/// while `updates` was loaded since: an info line, never a problem. dnf's
/// clock is each section's primary metadata file (dnf5 sets its time when
/// it confirms expired metadata in sync), so a source that only published
/// nothing new (its `repomd.xml` old, its primary file recent) is not
/// frozen, and a cache dir's own time, which moves for anything made in
/// it, says nothing.
#[tokio::test]
async fn a_software_source_that_stopped_loading_is_named() {
    const DAY: i128 = 86_400 * 1_000_000_000;
    // 2026-11-02 00:00 UTC.
    let nov_2 = 1_793_577_600 * 1_000_000_000_i128;
    let w = World::nvidia().await;
    let f = &w.m.facts;
    f.file(
        "/etc/yum.repos.d/nvidia-container-toolkit.repo",
        "[nvidia-container-toolkit]\nbaseurl=https://nvidia.github.io/libnvidia-container/stable/rpm/$basearch\nenabled=1\n",
    );
    let nv = "/var/cache/libdnf5/nvidia-container-toolkit-0000000000000001";
    let up = "/var/cache/libdnf5/updates-0000000000000002";
    let repomd = "<repomd><data type=\"primary\"><location href=\"repodata/p-primary.xml.gz\"/></data></repomd>";
    f.file(format!("{nv}/repodata/repomd.xml"), repomd)
        .touch(format!("{nv}/repodata/repomd.xml"), nov_2 - 43 * DAY)
        .file(format!("{nv}/repodata/p-primary.xml.gz"), b"")
        .touch(format!("{nv}/repodata/p-primary.xml.gz"), nov_2 - 19 * DAY)
        // Something made in the dir yesterday: not a load.
        .touch(nv, nov_2 - DAY)
        .file(format!("{up}/repodata/repomd.xml"), repomd)
        .touch(format!("{up}/repodata/repomd.xml"), nov_2)
        .file(format!("{up}/repodata/p-primary.xml.gz"), b"")
        .touch(format!("{up}/repodata/p-primary.xml.gz"), nov_2);
    let c = w.check().await;
    let toolkit = item(&c, MachineItemId::CardForModels);
    assert_eq!(toolkit.state, ItemState::Info, "{toolkit:#?}");
    assert!(matches!(
        &toolkit.cause,
        Some(MachineCause::ToolkitSourceFrozen { section, last_loaded })
            if section == "nvidia-container-toolkit" && last_loaded.starts_with("2026-10-14")
    ));
    assert!(
        toolkit.sentence.contains("since October 14, 2026."),
        "{}",
        toolkit.sentence
    );
    assert_eq!(c.verdict, MachineVerdict::Ready);

    // dnf loaded it yesterday, and NVIDIA published nothing new since
    // September.
    f.touch(format!("{nv}/repodata/p-primary.xml.gz"), nov_2 - DAY);
    let c = w.check().await;
    assert_eq!(item(&c, MachineItemId::CardForModels).state, ItemState::Ok);
}
