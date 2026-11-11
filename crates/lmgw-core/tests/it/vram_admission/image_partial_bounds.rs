//! What bounds a partial image start (`vram::partial`, third review of the
//! admission branch): the row's own `max_vram`, which wins at render; the
//! number of cards its engine sees, since `--max-vram` is per device; and a
//! failed generation, which says the working budget was not enough.

use super::image_partial_fit::{
    add_pipeline, draw, entry_warnings, image_resident, max_vram, mock_images, set_learned, start,
    MIB,
};
use super::*;
use lmgw_core::runtime::Class;
use lmgw_core::vram::Fit;

async fn refused(f: &Fixture, model_id: &str) -> String {
    let snap = f.state.snapshot();
    match f
        .state
        .vram
        .check_background_start(&f.state, &snap, Class::Image, model_id)
        .await
    {
        Fit::Full(why) => why,
        _ => panic!("{model_id} was not refused"),
    }
}

async fn patch(f: &Fixture, id: i64, fields: Value) {
    let mut p = json!({"action": "update", "id": id});
    for (k, v) in fields.as_object().unwrap() {
        p[k] = v.clone();
    }
    let p: lmgw_core::ops::ImageModelPatch = serde_json::from_value(p).unwrap();
    lmgw_core::ops::image_model_set(&f.state, p).await.unwrap();
}

/// Item 1: `max_vram 6` with 3 GiB free beyond the headroom beside an idle
/// chat model is not given those 3 GiB — sd-server would draw 6 and its
/// context. A Start is refused saying so; a request's start evicts the chat
/// model and is given its budget and its context.
#[tokio::test]
async fn an_own_budget_is_given_whole_or_takes_the_normal_path() {
    let f = fixture(8 * GIB, 9 * GIB / 2, GIB, 512).await;
    add_pipeline(
        &f,
        "big",
        [5 * GIB, 3 * GIB, GIB],
        json!({"offload_to_cpu": true, "max_vram": "6"}),
        0,
    )
    .await;
    drop(super::image_pipeline_peak::acquire_model(&f, Class::Chat, "chat-model").await);
    let why = refused(&f, "big").await;
    assert!(
        why.contains(
            "the max_vram in its args holds sd-server to 6.0 GiB, which with its context (1.0 \
             GiB) is more than the 3.0 GiB free beyond the headroom"
        ),
        "{why}"
    );
    assert!(!f.state.runtime().list().iter().any(|e| e.model_id == "big"));

    mock_images(&f).await;
    let resp = draw(&f, "big").await;
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    assert!(
        f.stops().contains(&"chat-model".to_string()),
        "the idle chat model made room: {:?}",
        f.stops()
    );
    assert_eq!(max_vram(&f, "big"), (Some("6".into()), 1));
    let v = vram_status(&f.gateway).await;
    assert_eq!(
        image_resident(&v)["estimated_bytes"],
        7 * GIB,
        "its own 6 GiB and the stand-in: {v}"
    );
    assert!(entry_warnings(&f, "big")
        .iter()
        .any(|w| w.contains("its own max_vram holds sd-server to 6.000 GiB")));
}

/// Item 1: an own `max_vram` of 0 or less follows the driver's free memory,
/// which bounds nothing: no partial grant, even with nothing else on the
/// card, and the row says why.
#[tokio::test]
async fn an_own_budget_of_zero_needs_the_whole() {
    let f = fixture(4 * GIB, GIB, GIB, 512).await;
    add_pipeline(
        &f,
        "turbo",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true, "max_vram": "0"}),
        0,
    )
    .await;
    let why = refused(&f, "turbo").await;
    assert!(
        why.contains("(0 or less) lets sd-server follow the driver's free memory"),
        "{why}"
    );
    let got = crate::common::read_wire(
        lmgw_core::ops::local_model_get(&f.state, None, Some("turbo"), Some("image"))
            .await
            .unwrap(),
    );
    let w = got["warnings"][0].as_str().unwrap_or_default();
    assert!(w.contains("which leaves it unbounded"), "{got}");
}

/// Item 5: `--max-vram` is per device and the ledger pools the cards, so a
/// partial grant needs exactly one card in the engine's view. Two named
/// cards take the whole-pipeline path, and say partial fitting on several
/// cards is not done yet; one named card fits partially.
#[tokio::test]
async fn on_several_cards_it_takes_the_whole_pipeline_path() {
    let f = fixture(4 * GIB, GIB, GIB, 512).await;
    let id = add_pipeline(
        &f,
        "turbo",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true}),
        0,
    )
    .await;
    patch(
        &f,
        id,
        json!({"extra_run_args": [
            "--device", "nvidia.com/gpu=0", "--device", "nvidia.com/gpu=1"
        ]}),
    )
    .await;
    let why = refused(&f, "turbo").await;
    assert!(
        why.contains("its engine would see 2 cards")
            && why.contains("fitting it partially on several cards is not done yet"),
        "{why}"
    );

    patch(
        &f,
        id,
        json!({"extra_run_args": ["--device", "nvidia.com/gpu=0"]}),
    )
    .await;
    let out = start(&f, "turbo").await;
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(max_vram(&f, "turbo"), (Some("2.500".into()), 1));
}

/// Item 3: a generation that fails under a grant at least the working
/// budget raises the figure to what sd.cpp names (its exact log line), and
/// clears it where sd.cpp names nothing; under less than the working budget
/// it teaches nothing.
#[tokio::test]
async fn a_failed_generation_raises_or_clears_the_working_budget() {
    let f = fixture(4 * GIB, GIB, GIB, 512).await;
    let id = add_pipeline(
        &f,
        "turbo",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true}),
        0,
    )
    .await;
    let work = || {
        f.state
            .snapshot()
            .image_models
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.work_bytes)
    };
    let set_work = |bytes: Option<u64>| {
        let f = &f;
        async move {
            set_learned(f, id, store::LearnedFigure::Work, bytes).await;
        }
    };
    let fail_once = || {
        let f = &f;
        async move {
            for m in [&f.first, &f._second, &f._third].into_iter().chain(&f.more) {
                Mock::given(method("POST"))
                    .and(path("/v1/images/generations"))
                    .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                        "error": "server_error", "message": "generation failed",
                    })))
                    .with_priority(1)
                    .up_to_n_times(1)
                    .mount(m)
                    .await;
            }
        }
    };
    mock_images(&f).await;
    // Up, held to 2.5 GiB: the budget it rendered with.
    assert_eq!(draw(&f, "turbo").await.status(), 200);
    assert_eq!(work(), Some(5 * GIB / 2));

    // A 512² picture taught 1 GiB; a larger one fails at 2.5 GiB, and
    // sd.cpp's split names 3000 MB: raised to that.
    set_work(Some(GIB)).await;
    f.world().logs.insert(
        "turbo".into(),
        "[INFO ] stable-diffusion.cpp:1210 - sampling using Euler method\n[ERROR] \
         layer_split_partition.cpp:166 - diffusion graph-cut layer split: segment 0 needs \
         3000.0 MB on CUDA0, but only 2560.0 MB is available under current VRAM limits\n"
            .into(),
    );
    fail_once().await;
    assert_eq!(draw(&f, "turbo").await.status(), 502);
    assert_eq!(work(), Some(3000 * MIB), "what sd.cpp named");

    // sd.cpp names nothing: not known again.
    set_work(Some(GIB)).await;
    f.world().logs.remove("turbo");
    fail_once().await;
    assert!(draw(&f, "turbo").await.status().is_server_error());
    assert_eq!(work(), None, "cleared");

    // Under less than the working budget: nothing new.
    set_work(Some(3 * GIB)).await;
    fail_once().await;
    assert!(draw(&f, "turbo").await.status().is_server_error());
    assert_eq!(work(), Some(3 * GIB));
}

/// Item 2: the learned figures belong to the engine they were learned under
/// (migration 0105). A working budget that lets the pipeline start beside
/// the chat model counts until the class's engine image switches (CUDA after
/// Vulkan) or the native launcher would start another build; then the start
/// takes the rule for a row with none learned. Switching back finds them,
/// and a figure learned under the new engine clears the old ones.
#[tokio::test]
async fn learned_figures_belong_to_the_engine() {
    let f = fixture_n(4 * GIB, 3 * GIB, GIB, 512, 1).await;
    let id = add_pipeline(
        &f,
        "huge",
        [4 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true}),
        0,
    )
    .await;
    drop(super::image_pipeline_peak::acquire_model(&f, Class::Chat, "chat-model").await);
    set_learned(&f, id, store::LearnedFigure::Idle, Some(5 * MIB)).await;
    set_learned(&f, id, store::LearnedFigure::Work, Some(256 * MIB)).await;
    let out = start(&f, "huge").await;
    assert_eq!(out["ok"], true, "{out}");
    assert!(
        out["warnings"][0]
            .as_str()
            .is_some_and(|w| w.contains("a budget of at least the 256 MiB it has rendered with")),
        "{out}"
    );
    f.state
        .runtime()
        .stop(Class::Image, "huge", false)
        .await
        .unwrap();

    // The class's engine image switches: not measured under it.
    let vulkan = f.state.snapshot().settings.image.image.clone();
    let set_image = |image: String| {
        let f = &f;
        async move {
            let mut s = f.state.snapshot().settings.clone();
            s.image.image = image;
            store::save_settings(&f.state.db, &s).await.unwrap();
            f.state.reload_snapshot().await.unwrap();
        }
    };
    set_image("ghcr.io/leejet/stable-diffusion.cpp:master-cuda12".into()).await;
    let why = refused(&f, "huge").await;
    assert!(why.contains("how little is not measured yet"), "{why}");
    set_image(vulkan).await;
    assert_eq!(start(&f, "huge").await["ok"], true, "found again");
    f.state
        .runtime()
        .stop(Class::Image, "huge", false)
        .await
        .unwrap();

    // The native launcher would start a CUDA build now: the same.
    f.state
        .vram
        .set_engine_build(Arc::new(|_| Some("master-1079-a3c49b1-cuda12".into())));
    let why = refused(&f, "huge").await;
    assert!(why.contains("how little is not measured yet"), "{why}");

    // Learned under that build: the old engine's figures go.
    set_learned(&f, id, store::LearnedFigure::Work, Some(512 * MIB)).await;
    let snap = f.state.snapshot();
    let row = snap.image_models.iter().find(|m| m.id == id).unwrap();
    assert_eq!((row.idle_bytes, row.work_bytes), (None, Some(512 * MIB)));
    assert_eq!(
        row.learned_key.as_deref(),
        Some(f.state.vram.image_engine_key(&snap, row).as_str())
    );
}
