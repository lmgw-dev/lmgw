//! The verdict's headline comes from the item that decided it (§1.7), the
//! toolkit from another repository is installed from there alone (§3.2),
//! and §1.4 tells render nodes from AMD's compute node and names the right
//! vendor's values.

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

fn install_helper(w: &World) {
    w.m.facts
        .file_mode(lmgw_machine::plan::embedded::HELPER, b"\x7fELF", 0o755)
        .file_mode(
            lmgw_machine::plan::embedded::POLICY,
            b"<policyconfig/>",
            0o644,
        )
        .file_mode(lmgw_machine::plan::embedded::PKEXEC, b"\x7fELF", 0o4755);
}

/// The toolkit is missing and the button could add it, but Podman is
/// missing too, which only instructions fix: the verdict is needs_steps and
/// its headline Podman's sentence, never the button's.
#[tokio::test]
async fn the_headline_is_the_problem_that_decided_the_verdict() {
    let w = World::nvidia().await;
    w.remove_toolkit();
    install_helper(&w);
    *w.podman.missing.lock().unwrap() = true;
    let c = w.check().await;
    assert_eq!(c.plan.as_ref().unwrap().how, PlanHow::Helper);
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps, "{c:#?}");
    assert_eq!(c.headline, "lmgw needs one more program to run models.");
    assert!(!c.headline.contains("lmgw can set it up"));

    // Settings that do not fit, with the helper's problem beside them: two
    // fixes with a button, and the helper's comes first (§1.2 before §1.4).
    *w.podman.missing.lock().unwrap() = false;
    w.settings(|s| {
        s.router.extra_run_args = vec![
            "--device".into(),
            "/dev/kfd".into(),
            "--device".into(),
            "/dev/dri".into(),
        ];
    })
    .await;
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::NeedsFix, "{c:#?}");
    assert_eq!(c.headline, item(&c, MachineItemId::CardForModels).sentence);
    // The detail gives the NVIDIA values as the switch would write them,
    // not AMD's document.
    let fit = item(&c, MachineItemId::SettingsFit);
    assert_eq!(fit.cause, Some(MachineCause::SettingsForAmd));
    assert_eq!(
        fit.sentence,
        "lmgw is set up for AMD cards. It can switch to the settings for NVIDIA cards."
    );
    assert!(
        fit.detail.iter().any(|l| l
            == "Settings → Runtimes → Chat → Extra podman run args: --device /dev/kfd --device \
                /dev/dri → --device nvidia.com/gpu=all --security-opt label=disable"),
        "{:?}",
        fit.detail
    );
    assert!(!fit.detail.iter().any(|l| l.contains("amd-arch")));
}

/// `/dev/dri` alone is what a Vulkan engine passes on any vendor's card:
/// not AMD's settings on an NVIDIA card (a detail line says what it lacks
/// there), and a fit on an AMD card.
#[tokio::test]
async fn render_nodes_alone_are_no_vendors_settings() {
    let w = World::nvidia().await;
    w.settings(|s| {
        s.router.extra_run_args = vec!["--device".into(), "/dev/dri".into()];
    })
    .await;
    let c = w.check().await;
    let fit = item(&c, MachineItemId::SettingsFit);
    assert_ne!(fit.state, ItemState::Problem, "{fit:#?}");
    assert!(
        fit.detail
            .iter()
            .any(|l| l.contains("passes only the render nodes (/dev/dri)")),
        "{:?}",
        fit.detail
    );

    // An AMD card with the Vulkan settings of docs/amd-arch.md fits; with
    // the render nodes and the CUDA images it does not (a CUDA image runs on
    // no AMD card).
    w.amd_card();
    w.settings(|s| {
        for args in [
            &mut s.router.extra_run_args,
            &mut s.aux_router.extra_run_args,
            &mut s.audio.extra_run_args,
            &mut s.image.extra_run_args,
        ] {
            *args = vec![
                "--device".into(),
                "/dev/dri".into(),
                "--group-add".into(),
                "keep-groups".into(),
            ];
        }
    })
    .await;
    let c = w.check().await;
    let fit = item(&c, MachineItemId::SettingsFit);
    assert_eq!(fit.state, ItemState::Problem, "{fit:#?}");
    assert_eq!(fit.cause, Some(MachineCause::SettingsForNvidia));
    w.settings(|s| {
        s.router.image = "ghcr.io/ggml-org/llama.cpp:server-vulkan".into();
        s.aux_router.image = "ghcr.io/ggml-org/llama.cpp:server-vulkan".into();
        s.audio.image = "ghcr.io/0xshug0/audio.cpp:full-vulkan".into();
        s.audio.backend = "vulkan".into();
        s.image.image = "ghcr.io/leejet/stable-diffusion.cpp:master-vulkan".into();
    })
    .await;
    let c = w.check().await;
    assert_eq!(
        item(&c, MachineItemId::SettingsFit).state,
        ItemState::Ok,
        "{c:#?}"
    );
}

/// Another enabled repository offers the toolkit (the user's cached
/// metadata says so): the instructions install it from there alone, with
/// no NVIDIA repository or key added, and the cause names it.
#[tokio::test]
async fn another_repository_offering_the_toolkit_is_installed_from() {
    let w = World::nvidia().await;
    w.remove_toolkit();
    install_helper(&w);
    let f = &w.m.facts;
    f.set_env("HOME", "/home/someone")
        .dir("/home/someone/.cache/libdnf5/vendor-toolkit-0123456789abcdef")
        .file(
            "/etc/yum.repos.d/vendor.repo",
            "[vendor-toolkit]\nname=A vendor\nbaseurl=https://vendor.example/rpm/\nenabled=1\n",
        )
        .program("/usr/bin/dnf5", |args| {
            assert_eq!(args[0], "repoquery");
            Ok(CmdOut::success(
                "nvidia-container-toolkit-base\t1.20.1-1\tvendor-toolkit\t4200000\n",
            ))
        });
    let c = w.check().await;
    let toolkit = item(&c, MachineItemId::CardForModels);
    assert_eq!(
        toolkit.cause,
        Some(MachineCause::ToolkitOtherProvider {
            repo: "vendor-toolkit".into()
        })
    );
    let plan = c.plan.as_ref().unwrap();
    let kinds: Vec<PlanStepKind> = plan.steps.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, [PlanStepKind::S4Install], "no NVIDIA source or key");
    assert_eq!(
        plan.steps[0].commands,
        [[
            "/usr/bin/dnf5",
            "install",
            "-y",
            "--repo=vendor-toolkit",
            "--setopt=vendor-toolkit.skip_if_unavailable=False",
            "--setopt=install_weak_deps=False",
            "nvidia-container-toolkit-base"
        ]]
    );
    assert!(plan.steps[0].terminal[0].starts_with("sudo dnf5 install -y --repo=vendor-toolkit "));
    let PlanHow::Instructions { why } = &plan.how else {
        panic!("{:?}", plan.how)
    };
    assert!(why.contains("(vendor-toolkit)"), "{why}");
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    assert_eq!(c.headline, toolkit.sentence);
}
