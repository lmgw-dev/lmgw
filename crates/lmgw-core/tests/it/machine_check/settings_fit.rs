//! The switch to the model settings for this computer's card (machine-check
//! design §1.4, §4.2, D18, WP12): the settings item offers it with every
//! field it changes, `machine_settings_fit` writes nothing without
//! `confirm` and then switches every class set for the other card through
//! the Settings page's save, the rows' own settings with it, and a check
//! after it finds the settings fit; the reverse on an NVIDIA card; no switch
//! offered or made for an Intel card or none.
//!
//! On `support/machine_world.rs`'s stated machines, over the op's HTTP
//! route; no settings of this computer's lmgw are read or written.
//! [`confirm`] holds the confirmation's contract and the running models
//! (D135, D136), [`cards`] the cards and images a setting is judged against
//! and the flags a round trip swaps (D137 to D139).

mod cards;
mod confirm;

use lmgw_api_types::machine::{
    ItemState, MachineAction, MachineCause, MachineCheck, MachineItem, MachineItemId,
    MachineSettingsFit, MachineVerdict, SettingsFit, SettingsFitChange, SettingsFitValue,
};
use lmgw_core::config::{AuxBackend, AuxKind};
use lmgw_core::store::{self, NewAuxModel, NewImageModel, NewLocalModel};
use serde_json::json;

use crate::common::{serve, Gw};
use crate::support::device_pair::op;
use crate::support::machine_world::World;

const CUDA: &str = "ghcr.io/ggml-org/llama.cpp:server-cuda";
const VULKAN: &str = "ghcr.io/ggml-org/llama.cpp:server-vulkan";
const AUDIO_CUDA: &str = "ghcr.io/0xshug0/audio.cpp:full-cuda12";
const AUDIO_VULKAN: &str = "ghcr.io/0xshug0/audio.cpp:full-vulkan";
const SD_CUDA: &str = "ghcr.io/leejet/stable-diffusion.cpp:master-cuda";
const SD_VULKAN: &str = "ghcr.io/leejet/stable-diffusion.cpp:master-vulkan";
const NVIDIA_ARGS: [&str; 4] = [
    "--device",
    "nvidia.com/gpu=all",
    "--security-opt",
    "label=disable",
];
const AMD_ARGS: [&str; 6] = [
    "--device",
    "/dev/dri",
    "--group-add",
    "keep-groups",
    "--security-opt",
    "label=disable",
];

fn v(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// An image or backend as a change shows it.
fn t(value: &str) -> Option<SettingsFitValue> {
    Some(SettingsFitValue::Text(value.into()))
}

/// Run arguments as a change shows them.
fn a(args: &[&str]) -> Option<SettingsFitValue> {
    Some(SettingsFitValue::Args(v(args)))
}

fn item(c: &MachineCheck, id: MachineItemId) -> &MachineItem {
    c.items.iter().find(|i| i.id == id).unwrap()
}

/// The switch the settings item offers.
pub(super) fn offered(c: &MachineCheck) -> (SettingsFit, Vec<SettingsFitChange>) {
    match &item(c, MachineItemId::SettingsFit).action {
        Some(MachineAction::SettingsFit { fit, changes, .. }) => (*fit, changes.clone()),
        None => panic!("no switch offered: {c:#?}"),
    }
}

/// Every image either card's settings name is on the machine, so neither
/// the download line nor the pre-fetch reaches for a registry.
fn every_image_here(w: &World) {
    let mut images = w.podman.images.lock().unwrap();
    for i in [
        CUDA,
        VULKAN,
        AUDIO_CUDA,
        AUDIO_VULKAN,
        SD_CUDA,
        SD_VULKAN,
        "localhost/sd-custom:cuda",
    ] {
        images.insert(i.to_string());
    }
}

/// `machine_settings_fit` for `fit`, confirming `confirm` when given.
async fn switch(
    gw: &Gw,
    fit: &str,
    confirm: Option<&[SettingsFitChange]>,
) -> (u16, serde_json::Value) {
    let body = match confirm {
        Some(c) => json!({ "fit": fit, "confirm": c }),
        None => json!({ "fit": fit }),
    };
    op(gw, "machine_settings_fit", body).await
}

/// The answer of a call that is not refused.
fn answer((status, body): (u16, serde_json::Value)) -> MachineSettingsFit {
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body).unwrap()
}

fn fields(changes: &[SettingsFitChange]) -> Vec<String> {
    changes
        .iter()
        .map(|c| match &c.model {
            Some(m) => format!("{m}: {}", c.field),
            None => c.field.clone(),
        })
        .collect()
}

fn chat(id: &str, image: Option<&str>, args: Option<&[&str]>, enabled: bool) -> NewLocalModel {
    NewLocalModel {
        model_id: id.into(),
        gguf_path: format!("{id}.gguf"),
        params: Default::default(),
        args: vec![],
        idle_seconds: 0,
        enabled,
        public: true,
        image: image.map(String::from),
        extra_run_args: args.map(v),
        warm_start: false,
        fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

/// An AMD card with lmgw's NVIDIA defaults: the item says so and offers the
/// switch, listing each class's image, run arguments and the audio backend
/// as they are and as they would become, by the Settings page's labels; the
/// verdict is a fix with a button, no password.
#[tokio::test]
async fn an_amd_card_with_nvidia_settings_is_offered_the_switch() {
    let w = World::amd().await;
    every_image_here(&w);
    let c = w.check().await;
    let fit = item(&c, MachineItemId::SettingsFit);
    assert_eq!(fit.state, ItemState::Problem, "{fit:#?}");
    assert_eq!(fit.cause, Some(MachineCause::SettingsForNvidia));
    assert_eq!(
        fit.sentence,
        "lmgw is set up for NVIDIA cards. It can switch to the settings for AMD cards."
    );
    assert_eq!(c.verdict, MachineVerdict::NeedsFix);
    assert_eq!(c.headline, fit.sentence);
    assert!(c.plan.is_none(), "no root step for an AMD card");

    let Some(MachineAction::SettingsFit {
        fit: to,
        label,
        changes,
        left,
        running,
    }) = &fit.action
    else {
        panic!("{fit:#?}")
    };
    assert_eq!(*to, SettingsFit::AmdVulkan);
    assert_eq!(label, "Switch to the settings for AMD cards");
    assert!(left.is_empty(), "{left:#?}");
    assert!(running.is_empty(), "nothing runs: {running:#?}");
    assert_eq!(
        fields(changes),
        [
            "Settings → Runtimes → Chat → Image",
            "Settings → Runtimes → Chat → Extra podman run args",
            "Settings → Runtimes → Aux → Image",
            "Settings → Runtimes → Aux → Extra podman run args",
            "Settings → Runtimes → Audio → Image",
            "Settings → Runtimes → Audio → Extra podman run args",
            "Settings → Runtimes → Audio → Backend",
            "Settings → Runtimes → Image → Image",
            "Settings → Runtimes → Image → Extra podman run args",
        ]
    );
    let by = |field: &str| changes.iter().find(|c| c.field == field).unwrap();
    let image = by("Settings → Runtimes → Chat → Image");
    assert_eq!((&image.from, &image.to), (&t(CUDA), &t(VULKAN)));
    let args = by("Settings → Runtimes → Chat → Extra podman run args");
    assert_eq!((&args.from, &args.to), (&a(&NVIDIA_ARGS), &a(&AMD_ARGS)));
    let backend = by("Settings → Runtimes → Audio → Backend");
    assert_eq!((&backend.from, &backend.to), (&t("cuda"), &t("vulkan")));
    assert_eq!(by("Settings → Runtimes → Image → Image").to, t(SD_VULKAN));
    // The detail says the same, by the same labels, and no longer sends the
    // person to a document.
    assert!(
        fit.detail
            .iter()
            .any(|l| l == &format!("Settings → Runtimes → Chat → Image: {CUDA} → {VULKAN}")),
        "{:?}",
        fit.detail
    );
    assert!(!fit.detail.iter().any(|l| l.contains("amd-arch")));
    assert!(!fit.detail.iter().any(|l| l.contains("from here yet")));
    // No config key is a label.
    for c in changes {
        assert!(!c.field.contains('_') && !c.field.contains('.'), "{c:?}");
    }
}

/// Without `confirm` nothing is written; with it every class is switched
/// through the Settings page's save, the answer lists the same fields and
/// carries a check that finds the settings fit, the other class settings
/// stay, and asking again has nothing left to switch.
#[tokio::test]
async fn the_switch_asks_first_then_writes_every_class_and_checks_again() {
    let w = World::amd().await;
    every_image_here(&w);
    let offered_changes = offered(&w.check().await).1;
    let gw = serve(w.state.clone()).await;
    let before = w.state.snapshot().settings.clone();

    let preview = answer(switch(&gw, "amd_vulkan", None).await);
    assert!(!preview.applied);
    assert!(preview.check.is_none());
    assert_eq!(preview.changes, offered_changes);
    assert!(
        preview
            .sentence
            .contains("Nothing is changed until it is confirmed."),
        "{}",
        preview.sentence
    );
    assert_eq!(
        w.state.snapshot().settings.router.image,
        CUDA,
        "nothing written"
    );

    let done = answer(switch(&gw, "amd_vulkan", Some(&preview.changes)).await);
    assert!(done.applied);
    assert_eq!(
        done.changes, offered_changes,
        "what was offered is what ran"
    );
    assert_eq!(done.outdated, None);
    assert_eq!(done.sentence, "lmgw now uses the settings for AMD cards.");
    assert!(
        done.notes
            .iter()
            .any(|n| n.contains("chat class definition changed")),
        "the Settings page's own notes: {:?}",
        done.notes
    );
    let s = w.state.snapshot().settings.clone();
    assert_eq!(
        (s.router.image.as_str(), &s.router.extra_run_args),
        (VULKAN, &v(&AMD_ARGS))
    );
    assert_eq!(
        (s.aux_router.image.as_str(), &s.aux_router.extra_run_args),
        (VULKAN, &v(&AMD_ARGS))
    );
    assert_eq!(
        (
            s.audio.image.as_str(),
            s.audio.backend.as_str(),
            &s.audio.extra_run_args
        ),
        (AUDIO_VULKAN, "vulkan", &v(&AMD_ARGS))
    );
    assert_eq!(
        (s.image.image.as_str(), &s.image.extra_run_args),
        (SD_VULKAN, &v(&AMD_ARGS))
    );
    // Nothing else of the classes moved.
    assert_eq!(s.router.public_prefix, before.router.public_prefix);
    assert_eq!(
        s.aux_router.decision_prefix,
        before.aux_router.decision_prefix
    );
    assert_eq!(s.audio.device, before.audio.device);
    assert_eq!(s.audio.threads, before.audio.threads);
    assert_eq!(
        s.image.request_timeout_seconds,
        before.image.request_timeout_seconds
    );

    let after = done.check.expect("a check after the write");
    let fit = item(&after, MachineItemId::SettingsFit);
    assert_eq!(fit.state, ItemState::Ok, "{fit:#?}");
    assert_eq!(fit.action, None);
    assert_eq!(after.verdict, MachineVerdict::Ready, "{after:#?}");
    assert_eq!(*w.state.machine.last().unwrap(), after, "published");

    let again = answer(switch(&gw, "amd_vulkan", Some(&done.changes)).await);
    assert!(!again.applied);
    assert!(again.changes.is_empty());
    assert_eq!(
        again.sentence,
        "The model settings already are the ones for AMD cards."
    );
}

/// Rows with settings of their own for NVIDIA cards follow the switch, in
/// its one save: a copy of the defaults is cleared, so the row follows its
/// class again; other arguments of a row's own stay with the device flags
/// swapped; a pinned CUDA image becomes the class's; a row on the processor
/// loses the card's flags and gets none; a disabled row is switched too; a
/// row already set for AMD cards is left alone; and an image lmgw doesn't
/// ship stays, named in what the switch leaves.
#[tokio::test]
async fn rows_with_their_own_settings_for_nvidia_cards_follow_the_switch() {
    let w = World::amd().await;
    every_image_here(&w);
    let db = &w.state.db;
    let shm: Vec<&str> = NVIDIA_ARGS
        .iter()
        .copied()
        .chain(["--shm-size", "8g"])
        .collect();
    let pinned = format!("{CUDA}@sha256:{}", "0".repeat(64));
    for row in [
        chat("copy", None, Some(&NVIDIA_ARGS), true),
        chat("shm", None, Some(&shm), true),
        chat("pinned", Some(&pinned), None, true),
        chat("off", None, Some(&NVIDIA_ARGS), false),
        chat("amd-own", None, Some(&AMD_ARGS), true),
    ] {
        store::insert_local_model(db, &row).await.unwrap();
    }
    store::insert_aux_model(
        db,
        &NewAuxModel {
            model_id: "cpu-embed".into(),
            gguf_path: "cpu-embed.gguf".into(),
            kind: AuxKind::Embed,
            pooling: None,
            ctx_size: None,
            ubatch_size: None,
            batch_size: None,
            mmproj_path: None,
            backend: Some(AuxBackend::Cpu),
            threads: None,
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: Some(v(&NVIDIA_ARGS)),
            warm_start: false,
            fallback: None,
        },
    )
    .await
    .unwrap();
    store::insert_image_model(
        db,
        &NewImageModel {
            model_id: "mine".into(),
            files: serde_json::from_value(json!({"diffusion_model": "mine/w.gguf"})).unwrap(),
            args: Default::default(),
            modes: vec![],
            edit: false,
            enabled: true,
            image: Some("localhost/sd-custom:cuda".into()),
            extra_run_args: None,
            warm_start: false,
            idle_seconds: 0,
            fallback: None,
            capabilities_override: None,
            prompt_style: None,
            preview_mode: None,
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();

    let c = w.check().await;
    let fit = item(&c, MachineItemId::SettingsFit);
    let Some(MachineAction::SettingsFit { changes, left, .. }) = &fit.action else {
        panic!("{fit:#?}")
    };
    // The cached result the item came from is the one the confirmation
    // compares with.
    assert_eq!(offered(&w.state.machine.last().unwrap()).1, *changes);
    let rows: Vec<String> = fields(changes)
        .into_iter()
        .filter(|f| !f.starts_with("Settings → "))
        .collect();
    assert_eq!(
        rows,
        [
            "copy: Extra podman run args override",
            "off: Extra podman run args override",
            "pinned: Image override",
            "shm: Extra podman run args override",
            "cpu-embed: Extra podman run args override",
        ],
        "{changes:#?}"
    );
    let copy = changes
        .iter()
        .find(|c| c.model.as_deref() == Some("copy"))
        .unwrap();
    assert_eq!(copy.to, None, "cleared: it follows its class again");
    assert_eq!(left.len(), 1, "{left:#?}");
    assert_eq!(left[0].model.as_deref(), Some("mine"));
    assert_eq!(left[0].field, "Image override");
    assert!(
        left[0]
            .why
            .contains("can't tell which card this image is built for"),
        "{}",
        left[0].why
    );
    assert!(
        fit.detail
            .iter()
            .any(|l| l.starts_with("model mine (image), Image override stays")),
        "{:?}",
        fit.detail
    );

    let gw = serve(w.state.clone()).await;
    let done = answer(switch(&gw, "amd_vulkan", Some(changes)).await);
    assert!(done.applied, "{done:#?}");
    assert_eq!(&done.changes, changes);
    let snap = w.state.snapshot();
    let row = |id: &str| {
        snap.local_models
            .iter()
            .find(|m| m.model_id == id)
            .unwrap()
            .clone()
    };
    assert_eq!(row("copy").extra_run_args, None);
    assert_eq!(row("off").extra_run_args, None);
    assert_eq!(
        row("shm").extra_run_args,
        Some(v(&[
            "--device",
            "/dev/dri",
            "--group-add",
            "keep-groups",
            "--security-opt",
            "label=disable",
            "--shm-size",
            "8g"
        ]))
    );
    assert_eq!(row("pinned").image, None);
    assert_eq!(row("amd-own").extra_run_args, Some(v(&AMD_ARGS)));
    assert_eq!(
        snap.aux_models[0].extra_run_args,
        Some(v(&["--security-opt", "label=disable"]))
    );
    assert_eq!(
        snap.image_models[0].image.as_deref(),
        Some("localhost/sd-custom:cuda")
    );
    let after = done.check.unwrap();
    assert_eq!(
        item(&after, MachineItemId::SettingsFit).state,
        ItemState::Ok,
        "{after:#?}"
    );
}

/// An NVIDIA card with settings for AMD cards: AMD's compute node (the
/// ROCm values of docs/amd-arch.md) is a problem with the switch back
/// offered; the Vulkan values lmgw's own switch writes are information with
/// the same offer (the render nodes alone are no vendor's, D51); switching
/// writes lmgw's NVIDIA defaults, and the check after it is ready.
#[tokio::test]
async fn an_nvidia_card_with_amd_settings_is_offered_the_way_back() {
    let w = World::nvidia().await;
    every_image_here(&w);
    w.settings(|s| {
        s.router.image = "ghcr.io/ggml-org/llama.cpp:server-rocm".into();
        s.router.extra_run_args = v(&[
            "--device",
            "/dev/kfd",
            "--device",
            "/dev/dri",
            "--group-add",
            "keep-groups",
            "--security-opt",
            "seccomp=unconfined",
        ]);
    })
    .await;
    let c = w.check().await;
    let fit = item(&c, MachineItemId::SettingsFit);
    assert_eq!(fit.state, ItemState::Problem, "{fit:#?}");
    assert_eq!(fit.cause, Some(MachineCause::SettingsForAmd));
    assert_eq!(
        fit.sentence,
        "lmgw is set up for AMD cards. It can switch to the settings for NVIDIA cards."
    );
    assert_eq!(c.verdict, MachineVerdict::NeedsFix);
    assert_eq!(c.headline, fit.sentence);
    let (to, changes) = offered(&c);
    assert_eq!(to, SettingsFit::NvidiaCuda);
    assert_eq!(
        fields(&changes),
        [
            "Settings → Runtimes → Chat → Image",
            "Settings → Runtimes → Chat → Extra podman run args"
        ]
    );

    // Every class as lmgw's switch to AMD's settings leaves it.
    w.settings(|s| {
        s.router.image = VULKAN.into();
        s.aux_router.image = VULKAN.into();
        s.audio.image = AUDIO_VULKAN.into();
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
    let c = w.check().await;
    let fit = item(&c, MachineItemId::SettingsFit);
    assert_eq!(fit.state, ItemState::Info, "{fit:#?}");
    assert_eq!(
        fit.sentence,
        "Some model settings are for AMD cards. lmgw can switch them to the settings for NVIDIA \
         cards."
    );
    assert_eq!(c.verdict, MachineVerdict::Ready);
    let (to, changes) = offered(&c);
    assert_eq!(to, SettingsFit::NvidiaCuda);
    assert_eq!(changes.len(), 9, "{changes:#?}");

    let gw = serve(w.state.clone()).await;
    let done = answer(switch(&gw, "nvidia_cuda", Some(&changes)).await);
    assert!(done.applied);
    let s = w.state.snapshot().settings.clone();
    let d = lmgw_core::config::Settings::default();
    assert_eq!(
        (&s.router.image, &s.router.extra_run_args),
        (&d.router.image, &d.router.extra_run_args)
    );
    assert_eq!(
        (&s.aux_router.image, &s.aux_router.extra_run_args),
        (&d.aux_router.image, &d.aux_router.extra_run_args)
    );
    assert_eq!(
        (&s.audio.image, &s.audio.backend, &s.audio.extra_run_args),
        (&d.audio.image, &d.audio.backend, &d.audio.extra_run_args)
    );
    assert_eq!(
        (&s.image.image, &s.image.extra_run_args),
        (&d.image.image, &d.image.extra_run_args)
    );
    let after = done.check.unwrap();
    assert_eq!(after.verdict, MachineVerdict::Ready);
    let fit = item(&after, MachineItemId::SettingsFit);
    assert_eq!(fit.state, ItemState::Ok, "{fit:#?}");
    assert_eq!(fit.action, None);
}

/// A class the person set to run on the processor stays so, and the switch
/// says it leaves it.
#[tokio::test]
async fn a_class_on_the_processor_is_left_as_it_is() {
    let w = World::amd().await;
    every_image_here(&w);
    w.settings(|s| s.audio.extra_run_args = v(&["--security-opt", "label=disable"]))
        .await;
    let c = w.check().await;
    let Some(MachineAction::SettingsFit { changes, left, .. }) =
        &item(&c, MachineItemId::SettingsFit).action
    else {
        panic!("{c:#?}")
    };
    assert!(
        !fields(changes).iter().any(|f| f.contains("Audio")),
        "{changes:#?}"
    );
    assert_eq!(left.len(), 1, "{left:#?}");
    assert_eq!(
        left[0].field,
        "Settings → Runtimes → Audio → Extra podman run args"
    );
    assert!(
        left[0].why.contains("run on the processor"),
        "{}",
        left[0].why
    );
}

/// An Intel card, and no card at all: no switch is offered, and the op is
/// refused with a plain reason, writing nothing.
#[tokio::test]
async fn no_switch_for_an_intel_card_or_none() {
    let w = World::nvidia().await;
    every_image_here(&w);
    w.intel_card();
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::CloudOnly);
    assert!(c.items.iter().all(|i| i.action.is_none()), "{c:#?}");
    let gw = serve(w.state.clone()).await;
    for fit in ["amd_vulkan", "nvidia_cuda"] {
        let (status, body) = switch(&gw, fit, Some(&[])).await;
        assert_eq!(status, 400, "{body}");
        assert!(
            body.to_string().contains(
                "No graphics card lmgw can use was found, so there are no settings to switch to."
            ),
            "{body}"
        );
    }
    let s = w.state.snapshot().settings.clone();
    assert_eq!(s.router.image, CUDA, "nothing written");

    w.no_card();
    let (status, body) = switch(&gw, "amd_vulkan", None).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("No graphics card"), "{body}");

    // The other maker's settings on an NVIDIA card: refused by name.
    let w = World::nvidia().await;
    let gw = serve(w.state.clone()).await;
    let (status, body) = switch(&gw, "amd_vulkan", Some(&[])).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.to_string().contains(
            "This computer has no AMD graphics card, so lmgw doesn't switch to the settings for \
             AMD cards."
        ),
        "{body}"
    );
    // A field it does not know is refused by name.
    let (status, body) = op(&gw, "machine_settings_fit", json!({ "fti": "amd_vulkan" })).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("fti"), "{body}");
}
