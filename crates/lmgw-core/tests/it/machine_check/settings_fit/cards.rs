//! What a setting is judged against (D137 to D139): the cards on this
//! computer (an AMD card beside an NVIDIA one, an AMD card without its
//! compute node), what an image of the owner's is built for (its build
//! label, its libraries), and the flags a round trip swaps, a class's and a
//! row's alike, with a group of the owner's left as it is.

use lmgw_api_types::machine::{
    ItemState, MachineAction, MachineCause, MachineItemId, MachineVerdict,
};
use lmgw_core::store::{self, NewImageModel};
use serde_json::json;

use super::{
    a, answer, chat, every_image_here, fields, item, offered, switch, t, v, AMD_ARGS, CUDA,
    NVIDIA_ARGS, SD_VULKAN, VULKAN,
};
use crate::common::serve;
use crate::support::machine_world::World;

const ROCM: &str = "ghcr.io/ggml-org/llama.cpp:server-rocm";
const ROCM_ARGS: [&str; 8] = [
    "--device",
    "/dev/kfd",
    "--device",
    "/dev/dri",
    "--group-add",
    "keep-groups",
    "--security-opt",
    "seccomp=unconfined",
];

/// An image row with image `image` of its own, and `args` of its own.
fn image_row(id: &str, image: &str, args: Option<&[&str]>) -> NewImageModel {
    NewImageModel {
        model_id: id.into(),
        files: serde_json::from_value(json!({"diffusion_model": format!("{id}/w.gguf")})).unwrap(),
        args: Default::default(),
        modes: vec![],
        edit: false,
        enabled: true,
        image: Some(image.into()),
        extra_run_args: args.map(v),
        warm_start: false,
        idle_seconds: 0,
        fallback: None,
        capabilities_override: None,
        prompt_style: None,
        preview_mode: None,
    }
}

/// On a computer with both cards, either switch is allowed (D116), and a
/// round trip ends where it began (D138): the chat class with arguments of
/// the owner's goes to AMD's settings and back with them kept; a row's own
/// arguments likewise; a row's copy of the class is cleared and stays so;
/// a row with ROCm's values ends on NVIDIA's, its user's groups and seccomp
/// profile gone; a row with a group of its own is left as it is, and the
/// switch says why.
#[tokio::test]
async fn both_cards_switch_there_and_back_and_end_where_they_began() {
    let w = World::nvidia().await;
    w.add_amd_card();
    every_image_here(&w);
    let extra = ["--shm-size", "16g", "-e", "GREETING=hello there"];
    let mine: Vec<&str> = NVIDIA_ARGS.iter().copied().chain(extra).collect();
    w.settings(|s| s.router.extra_run_args = v(&mine)).await;
    let ipc: Vec<&str> = NVIDIA_ARGS
        .iter()
        .copied()
        .chain(["--ipc", "host"])
        .collect();
    let grouped: Vec<&str> = NVIDIA_ARGS
        .iter()
        .copied()
        .chain(["--group-add", "video"])
        .collect();
    for row in [
        chat("ipc", None, Some(&ipc), true),
        chat("copy", None, Some(&mine), true),
        chat("grouped", None, Some(&grouped), true),
        chat("rocm", None, Some(&ROCM_ARGS), true),
    ] {
        store::insert_local_model(&w.state.db, &row).await.unwrap();
    }
    w.state.reload_snapshot().await.unwrap();
    let gw = serve(w.state.clone()).await;
    let row = |id: &str| {
        w.state
            .snapshot()
            .local_models
            .iter()
            .find(|m| m.model_id == id)
            .unwrap()
            .extra_run_args
            .clone()
    };

    let there = answer(switch(&gw, "amd_vulkan", None).await);
    let grouped_left = there
        .left
        .iter()
        .find(|l| l.model.as_deref() == Some("grouped"))
        .expect("the row with a group of its own is left");
    assert!(
        grouped_left.why.contains("a group of their own (video)"),
        "{}",
        grouped_left.why
    );
    let done = answer(switch(&gw, "amd_vulkan", Some(&there.changes)).await);
    assert!(done.applied, "{done:#?}");
    let theirs: Vec<&str> = AMD_ARGS.iter().copied().chain(extra).collect();
    assert_eq!(
        w.state.snapshot().settings.router.extra_run_args,
        v(&theirs)
    );
    let ipc_amd: Vec<&str> = AMD_ARGS.iter().copied().chain(["--ipc", "host"]).collect();
    assert_eq!(row("ipc"), Some(v(&ipc_amd)));
    assert_eq!(row("copy"), None, "a copy of the class follows it again");
    assert_eq!(row("grouped"), Some(v(&grouped)), "left as it was");

    let back = answer(switch(&gw, "nvidia_cuda", None).await);
    let done = answer(switch(&gw, "nvidia_cuda", Some(&back.changes)).await);
    assert!(done.applied, "{done:#?}");
    let s = w.state.snapshot().settings.clone();
    assert_eq!(s.router.extra_run_args, v(&mine), "the class as it began");
    assert_eq!(s.router.image, CUDA);
    assert_eq!(row("ipc"), Some(v(&ipc)), "the row as it began");
    assert_eq!(row("copy"), None);
    assert_eq!(row("rocm"), Some(v(&NVIDIA_ARGS)), "no AMD flag left");
    assert_eq!(row("grouped"), Some(v(&grouped)));
}

/// An image of the owner's on an AMD card is judged by what it is (D139):
/// built for CUDA by its label, or by the CUDA library in it, it keeps
/// models from running and the switch cannot change it, so the verdict is
/// steps and the detail names it; labelled Vulkan, or one lmgw cannot tell,
/// it fits.
#[tokio::test]
async fn an_image_of_the_owners_is_judged_by_its_label_or_its_libraries() {
    let w = World::amd().await;
    every_image_here(&w);
    w.settings(|s| {
        s.router.image = VULKAN.into();
        s.aux_router.image = VULKAN.into();
        s.audio.image = "ghcr.io/0xshug0/audio.cpp:full-vulkan".into();
        s.audio.backend = "vulkan".into();
        s.image.image = SD_VULKAN.into();
        for args in [
            &mut s.router.extra_run_args,
            &mut s.aux_router.extra_run_args,
            &mut s.audio.extra_run_args,
            &mut s.image.extra_run_args,
        ] {
            *args = v(&AMD_ARGS);
        }
    })
    .await;
    let labelled = "localhost/sd-custom:cuda";
    let listed = "localhost/sd-other:latest";
    w.podman.images.lock().unwrap().insert(listed.into());
    store::insert_image_model(&w.state.db, &image_row("mine", labelled, None))
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let set_label = |backend: &str| {
        w.podman.labels.lock().unwrap().insert(
            labelled.into(),
            [("dev.lmgw.backend".to_string(), backend.to_string())].into(),
        );
    };

    set_label("cuda");
    let c = w.check().await;
    let fit = item(&c, MachineItemId::SettingsFit);
    assert_eq!(fit.state, ItemState::Problem, "{fit:#?}");
    assert_eq!(fit.cause, Some(MachineCause::SettingsForNvidia));
    assert_eq!(fit.action, None, "nothing the switch can change");
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    assert_eq!(
        fit.sentence,
        "Some model settings are for NVIDIA cards, and lmgw can't switch all of them to the \
         settings for AMD cards. The details say which to change by hand."
    );
    assert!(
        fit.detail.iter().any(|l| l
            == "model mine (image), Image override stays localhost/sd-custom:cuda: it is built \
                for NVIDIA cards (CUDA) and isn't one of lmgw's images, so lmgw doesn't replace \
                it: pick one for AMD cards on the model's page"),
        "{:#?}",
        fit.detail
    );

    set_label("vulkan");
    let c = w.check().await;
    assert_eq!(
        item(&c, MachineItemId::SettingsFit).state,
        ItemState::Ok,
        "{c:#?}"
    );
    assert_eq!(c.verdict, MachineVerdict::Ready);

    // No label: its libraries say.
    w.podman.labels.lock().unwrap().clear();
    w.podman.libraries.lock().unwrap().insert(
        listed.into(),
        "/app/libggml-base.so\n/app/libggml-cpu-zen4.so\n/app/libggml-cuda.so\n".into(),
    );
    sqlx::query("UPDATE image_models SET image = ?1 WHERE model_id = 'mine'")
        .bind(listed)
        .execute(&w.state.db)
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps, "{c:#?}");
    // Nothing that tells: it may be built for this card.
    w.podman.libraries.lock().unwrap().clear();
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::Ready, "{c:#?}");
}

/// An AMD card on amdgpu with its render nodes and no `/dev/kfd`: the card
/// is ready (a Vulkan engine computes through the render nodes), ROCm's
/// settings do not fit it, and the switch to the Vulkan ones is offered
/// and makes it ready (D137).
#[tokio::test]
async fn an_amd_card_without_its_compute_node_is_offered_vulkan() {
    let w = World::amd().await;
    w.amd_card_without_kfd();
    every_image_here(&w);
    w.podman.images.lock().unwrap().insert(ROCM.into());
    w.settings(|s| {
        s.router.image = ROCM.into();
        s.router.extra_run_args = v(&ROCM_ARGS);
    })
    .await;
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    assert_eq!(card.state, ItemState::Ok, "{card:#?}");
    assert!(
        card.detail
            .iter()
            .any(|l| l.contains("/dev/kfd: not there")),
        "{:?}",
        card.detail
    );
    let fit = item(&c, MachineItemId::SettingsFit);
    assert_eq!(fit.state, ItemState::Problem, "{fit:#?}");
    assert_eq!(c.verdict, MachineVerdict::NeedsFix);
    let (_, changes) = offered(&c);
    let chat = |field: &str| {
        changes
            .iter()
            .find(|c| c.field == field)
            .unwrap_or_else(|| panic!("{field}: {changes:#?}"))
            .clone()
    };
    assert_eq!(chat("Settings → Runtimes → Chat → Image").to, t(VULKAN));
    assert_eq!(
        chat("Settings → Runtimes → Chat → Extra podman run args").to,
        a(&[
            "--device",
            "/dev/dri",
            "--security-opt",
            "label=disable",
            "--group-add",
            "keep-groups"
        ])
    );

    let gw = serve(w.state.clone()).await;
    let done = answer(switch(&gw, "amd_vulkan", Some(&changes)).await);
    assert_eq!(done.check.unwrap().verdict, MachineVerdict::Ready);
}

/// An NVIDIA card first and an AMD card beside it (D137): a row set for
/// the AMD card (ROCm's values, its compute node there) is no problem and
/// the machine is ready; the switch that would move it is information. The
/// AMD card's settings may be asked for on this computer (D116).
#[tokio::test]
async fn a_row_on_the_amd_card_beside_an_nvidia_one_is_no_problem() {
    let w = World::nvidia().await;
    w.add_amd_card();
    every_image_here(&w);
    store::insert_local_model(
        &w.state.db,
        &chat("rocm-row", Some(ROCM), Some(&ROCM_ARGS), true),
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let c = w.check().await;
    let fit = item(&c, MachineItemId::SettingsFit);
    assert_eq!(fit.state, ItemState::Info, "{fit:#?}");
    assert_eq!(c.verdict, MachineVerdict::Ready, "{c:#?}");
    assert!(
        fit.detail.iter().any(
            |l| l == "an AMD card is here too (render nodes and /dev/kfd): settings for it fit"
        ),
        "{:?}",
        fit.detail
    );
    let Some(MachineAction::SettingsFit { changes, .. }) = &fit.action else {
        panic!("{fit:#?}")
    };
    assert!(
        fields(changes).contains(&"rocm-row: Image override".to_string()),
        "{changes:#?}"
    );

    let gw = serve(w.state.clone()).await;
    let preview = answer(switch(&gw, "amd_vulkan", None).await);
    assert!(!preview.applied);
    assert!(!preview.changes.is_empty(), "{preview:#?}");
}

/// On an NVIDIA card, the render nodes with an image lmgw can't tell are
/// left and named alike on a class and on a row (L7); and the switch's
/// information keeps the processor's sentence beside it (L3).
#[tokio::test]
async fn the_render_nodes_are_left_alike_and_every_information_is_said() {
    let w = World::nvidia().await;
    every_image_here(&w);
    w.podman
        .images
        .lock()
        .unwrap()
        .insert("localhost/mine:latest".into());
    w.settings(|s| {
        s.router.image = "localhost/mine:latest".into();
        s.router.extra_run_args = v(&["--device", "/dev/dri"]);
        s.image.image = SD_VULKAN.into();
        s.image.extra_run_args = v(&AMD_ARGS);
        s.audio.extra_run_args = v(&["--security-opt", "label=disable"]);
    })
    .await;
    store::insert_local_model(
        &w.state.db,
        &chat(
            "own-dri",
            Some("localhost/mine:latest"),
            Some(&["--device", "/dev/dri", "--shm-size", "1g"]),
            true,
        ),
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let c = w.check().await;
    let fit = item(&c, MachineItemId::SettingsFit);
    assert_eq!(fit.state, ItemState::Info, "{fit:#?}");
    assert_eq!(c.verdict, MachineVerdict::Ready);
    assert_eq!(
        fit.sentence,
        "Some model settings are for AMD cards. lmgw can switch them to the settings for NVIDIA \
         cards. Speech and audio models are set to run on the processor."
    );
    let Some(MachineAction::SettingsFit { left, .. }) = &fit.action else {
        panic!("{fit:#?}")
    };
    let why = |model: Option<&str>| {
        left.iter()
            .find(|l| l.model.as_deref() == model && l.field.contains("run args"))
            .map(|l| l.why.clone())
            .unwrap_or_else(|| panic!("{model:?}: {left:#?}"))
    };
    assert_eq!(why(None), why(Some("own-dri")));
    assert!(
        why(None).contains("only the graphics card's video devices"),
        "{}",
        why(None)
    );
    assert!(!why(None).contains("render"), "plain words: {}", why(None));
}
