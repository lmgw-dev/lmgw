//! An image pipeline that can run with less than all of it on the card is
//! charged the whole wherever it fits, and otherwise starts with what
//! admission gives it — held to that with `--max-vram`, charged it by the
//! ledger, and saying so in a warning its entry keeps (`vram::partial`, the
//! owner's ruling of 2026-11-11). Refused is what really cannot run: a
//! pipeline that pins its weights on the card and is larger than it, and a
//! start short of its measured floor.

use super::*;
use lmgw_core::runtime::registry::PartialGrant;
use lmgw_core::runtime::Class;
use lmgw_core::vram::Fit;

pub(super) const MIB: u64 = 1024 * 1024;
pub(super) const LEAD: &str = "It will work but only fits partially on the GPU, expect slowdowns:";

/// A pipeline of three files in the shared models dir — a diffusion model,
/// a text encoder, a VAE — whose row sets `args`. Its container holds
/// `idle` on the fake card once it is up.
pub(super) async fn add_pipeline(
    f: &Fixture,
    model_id: &str,
    files: [u64; 3],
    args: Value,
    idle: u64,
) -> i64 {
    let mut map = serde_json::Map::new();
    for (key, bytes) in ["diffusion_model", "llm", "vae"].into_iter().zip(files) {
        let file = format!("{model_id}-{key}.gguf");
        crate::support::sparse::create(f._models_dir.path().join(&file), bytes);
        map.insert(key.into(), json!(file));
    }
    f.world().size.insert(model_id.to_string(), idle);
    let id = store::insert_image_model(
        &f.state.db,
        &store::NewImageModel {
            model_id: model_id.to_string(),
            files: map,
            args: args.as_object().cloned().unwrap_or_default(),
            enabled: true,
            idle_seconds: 0,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    id
}

pub(super) async fn start(f: &Fixture, model_id: &str) -> Value {
    crate::common::container_wire(
        lmgw_core::ops::container(&f.state, None, Some(model_id), "start", false, None)
            .await
            .unwrap(),
    )
}

pub(super) async fn stop(f: &Fixture, model_id: &str) {
    f.state
        .runtime()
        .stop(Class::Image, model_id, false)
        .await
        .unwrap();
}

pub(super) fn entry_warnings(f: &Fixture, model_id: &str) -> Vec<String> {
    f.state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.class == Class::Image && v.model_id == model_id)
        .expect("it is up")
        .warnings
}

/// The value of `--max-vram` in `model_id`'s last `podman run`, and how often
/// the flag is there.
pub(super) fn max_vram(f: &Fixture, model_id: &str) -> (Option<String>, usize) {
    let argv = f.world().argv.get(model_id).cloned().unwrap_or_default();
    let at: Vec<usize> = argv
        .iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--max-vram")
        .map(|(i, _)| i)
        .collect();
    (at.first().and_then(|i| argv.get(i + 1).cloned()), at.len())
}

pub(super) fn image_resident(v: &Value) -> Value {
    v["resident"]
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["container"] == "image").cloned())
        .unwrap_or_else(|| panic!("no image resident in {v}"))
}

pub(super) async fn learn(f: &Fixture, id: i64, idle: Option<u64>, peak: Option<u64>) {
    set_learned(f, id, store::LearnedFigure::Idle, idle).await;
    set_learned(f, id, store::LearnedFigure::Peak, peak).await;
}

/// The engine key image row `id` resolves to now.
pub(super) fn key_now(f: &Fixture, id: i64) -> String {
    let snap = f.state.snapshot();
    let row = snap.image_models.iter().find(|m| m.id == id).unwrap();
    f.state.vram.image_engine_key(&snap, row)
}

/// One learned figure of row `id`, as learned under the engine it resolves
/// to now (migration 0105).
pub(super) async fn set_learned(
    f: &Fixture,
    id: i64,
    figure: store::LearnedFigure,
    value: Option<u64>,
) {
    store::set_image_model_learned(&f.state.db, id, &key_now(f, id), figure, value)
        .await
        .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

pub(super) async fn mock_images(f: &Fixture) {
    for m in [&f.first, &f._second, &f._third].into_iter().chain(&f.more) {
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "created": 1, "data": [{"b64_json": "aW1n"}],
            })))
            .mount(m)
            .await;
    }
}

pub(super) async fn draw(f: &Fixture, model_id: &str) -> reqwest::Response {
    f.gateway
        .client()
        .post(format!("{}/v1/images/generations", f.gateway))
        .json(&json!({"model": format!("image/{model_id}"), "prompt": "a cat"}))
        .send()
        .await
        .unwrap()
}

/// Fix 2 and 5 of the review: where the whole fits, an offloaded pipeline is
/// charged the whole — its largest file is no upper bound for what one
/// generation takes — with no warning and no `--max-vram`. Once its idle
/// residency and peak are learned it is charged both, in the budget-only
/// branch too.
#[tokio::test]
async fn a_pipeline_whose_whole_fits_is_charged_the_whole() {
    let f = fixture(8 * GIB, 4 * GIB, GIB, 512).await;
    let id = add_pipeline(
        &f,
        "turbo",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true}),
        256 * MIB,
    )
    .await;
    let out = start(&f, "turbo").await;
    assert_eq!(out["ok"], true, "{out}");
    assert!(out.get("warnings").is_none(), "{out}");
    assert!(entry_warnings(&f, "turbo").is_empty());
    assert_eq!(max_vram(&f, "turbo"), (None, 0), "sized as it always was");

    let v = vram_status(&f.gateway).await;
    let r = image_resident(&v);
    assert_eq!(r["estimated_bytes"], 5 * GIB + 512 * MIB, "the whole: {r}");
    assert_eq!(
        v["free_bytes"],
        8 * GIB - 256 * MIB - (5 * GIB + 512 * MIB),
        "the driver shows its idle 256 MiB; the rest of the whole is kept free: {v}"
    );

    // Learned: its idle residency and the peak above it are its charge.
    learn(&f, id, Some(256 * MIB), Some(2 * GIB)).await;
    let v = vram_status(&f.gateway).await;
    let r = image_resident(&v);
    assert_eq!(r["estimated_bytes"], 2 * GIB + 256 * MIB, "{r}");
    assert_eq!(v["kept_free"]["image_peak_bytes"], 2 * GIB, "{v}");
    assert_eq!(v["free_bytes"], 8 * GIB - 256 * MIB - 2 * GIB, "{v}");

    // A chat model of 4 GiB fits beside it, and nothing is evicted.
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert!(f.stops().is_empty(), "{:?}", f.stops());

    // No telemetry, a declared budget: idle + peak and the chat model.
    let mut s = f.state.snapshot().settings.clone();
    s.vram.budget_mb = 8 * 1024;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    f.state
        .vram
        .set_probe(Arc::new(lmgw_core::vram::nvml::NoTelemetry(
            "no driver on this host".into(),
        )));
    let v = vram_status(&f.gateway).await;
    assert_eq!(v["free_measured"], false);
    assert_eq!(
        v["estimated_resident_bytes"],
        2 * GIB + 256 * MIB + 4 * GIB,
        "the idle residency is counted: {v}"
    );
    assert_eq!(v["free_bytes"], 8 * GIB - (6 * GIB + 256 * MIB), "{v}");
    assert_eq!(v["kept_free"]["image_peak_bytes"], 0);
}

/// Fix 1 and 6, and item 2 of the second review: a Windows test card in round
/// figures — 4 GiB, 512 MiB headroom, 5.5 GiB of files with
/// `offload_to_cpu`, no context measured. Its on-card part (its largest file
/// and the 1.0 GiB set aside for its context) does not fit, nothing else is
/// on the card and no fallback answers, so it starts with all that is free,
/// held to that less the stand-in, is charged it, and says so — from the
/// operator's Start and from a request.
#[tokio::test]
async fn a_pipeline_larger_than_the_card_starts_held_to_its_grant() {
    let f = fixture(4 * GIB, GIB, GIB, 512).await;
    add_pipeline(
        &f,
        "turbo",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true}),
        0,
    )
    .await;

    // The row says it before anything runs.
    let got = crate::common::read_wire(
        lmgw_core::ops::local_model_get(&f.state, None, Some("turbo"), Some("image"))
            .await
            .unwrap(),
    );
    let w = got["warnings"][0].as_str().unwrap_or_default();
    assert!(
        w.starts_with(LEAD)
            && w.contains("5.5 GiB in full plus 512 MiB headroom and the GPU holds 4.0 GiB")
            && w.contains("offload_to_cpu keeps its weights in RAM")
            && w.contains(
                "gives it 4.0 GiB on the card (its largest file, diffusion_model, and 1.0 GiB \
                 set aside for its context, not measured yet)"
            ),
        "{got}"
    );

    let out = start(&f, "turbo").await;
    assert_eq!(out["ok"], true, "{out}");
    let w = out["warnings"][0].as_str().unwrap_or_default().to_string();
    assert!(
        w.starts_with(LEAD)
            && w.contains("needs 5.5 GiB in full plus 512 MiB headroom and 4.0 GiB is free")
            && w.contains(
                "gives it 3.5 GiB, all that is free beyond the headroom: short of the 4.0 GiB"
            )
            && w.contains("with nothing else of lmgw's on the card to wait for and no fallback")
            && w.contains("holds sd-server to 2.500 GiB (--max-vram)")
            && w.contains(
                "1.0 GiB set aside for its context outside that budget, not measured yet"
            ),
        "{out}"
    );
    assert!(entry_warnings(&f, "turbo").contains(&w));
    assert_eq!(max_vram(&f, "turbo"), (Some("2.500".into()), 1));

    // The ledger charges what it was given: its budget and the stand-in.
    let v = vram_status(&f.gateway).await;
    let r = image_resident(&v);
    assert_eq!(r["estimated_bytes"], 3 * GIB + GIB / 2, "{r}");
    assert_eq!(v["free_bytes"], GIB / 2, "{v}");

    // A request's start, after a stop: the same grant and warning.
    stop(&f, "turbo").await;
    mock_images(&f).await;
    let resp = draw(&f, "turbo").await;
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    assert!(entry_warnings(&f, "turbo")
        .iter()
        .any(|x| x.starts_with(LEAD) && x.contains("--max-vram")));
    assert_eq!(max_vram(&f, "turbo"), (Some("2.500".into()), 1));
}

/// Item 2 of the second review: a context read after the grant was decided
/// is charged on top of the budget where that is more than the grant — what
/// sd-server can draw, not what admission thought.
#[tokio::test]
async fn a_context_larger_than_the_stand_in_is_charged_beside_the_budget() {
    let f = fixture(4 * GIB, GIB, GIB, 512).await;
    f.attribute(0);
    let id = add_pipeline(
        &f,
        "turbo",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true}),
        3 * GIB / 2,
    )
    .await;
    let out = start(&f, "turbo").await;
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(max_vram(&f, "turbo"), (Some("2.500".into()), 1));
    let idle = || {
        f.state
            .snapshot()
            .image_models
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.idle_bytes)
    };
    for _ in 0..200 {
        if idle().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(idle(), Some(3 * GIB / 2), "its context, read at rest");
    let v = vram_status(&f.gateway).await;
    let r = image_resident(&v);
    assert_eq!(
        r["estimated_bytes"],
        4 * GIB,
        "2.5 GiB budget + 1.5 GiB context: {r}"
    );
    assert_eq!(v["free_bytes"], 0, "{v}");
}

/// Fix 1, and item 1 of the third review: a `max_vram` in the row's args
/// wins at render, so admission gives such a row that budget and its
/// context or nothing partial — never less than sd-server can draw.
#[tokio::test]
async fn the_rows_own_max_vram_wins() {
    let f = fixture(4 * GIB, GIB, GIB, 512).await;
    add_pipeline(
        &f,
        "turbo",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true, "max-vram": "2"}),
        0,
    )
    .await;
    let out = start(&f, "turbo").await;
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(max_vram(&f, "turbo"), (Some("2".into()), 1));
    let w = entry_warnings(&f, "turbo");
    assert!(
        w.iter().any(|w| w.contains(
            "gives it 3.0 GiB on the card, the budget the max_vram in its args sets and its \
             context, and its own max_vram holds sd-server to 2.000 GiB (--max-vram)"
        )),
        "{w:?}"
    );
    assert!(
        !w.iter()
            .any(|w| lmgw_core::runtime::registry::is_unheld_note(w)),
        "held to what admission gave it: {w:?}"
    );
    // Charged its own 2 GiB and the stand-in.
    let v = vram_status(&f.gateway).await;
    assert_eq!(image_resident(&v)["estimated_bytes"], 3 * GIB);
    stop(&f, "turbo").await;

    // An own budget that with its context is more than is free: no partial
    // grant of less, the normal path — a Start refused, saying why.
    let snap = f.state.snapshot();
    let row = snap
        .image_models
        .iter()
        .find(|m| m.model_id == "turbo")
        .unwrap();
    let patch: lmgw_core::ops::ImageModelPatch = serde_json::from_value(json!({
        "action": "update",
        "id": row.id,
        "args": {"offload_to_cpu": true, "max-vram": "cuda0=3,vulkan0=2"},
    }))
    .unwrap();
    lmgw_core::ops::image_model_set(&f.state, patch)
        .await
        .unwrap();
    let snap = f.state.snapshot();
    match f
        .state
        .vram
        .check_background_start(&f.state, &snap, Class::Image, "turbo")
        .await
    {
        Fit::Full(why) => assert!(
            why.contains(
                "the max_vram in its args holds sd-server to 3.0 GiB, which with its context \
                 (1.0 GiB) is more than the 3.5 GiB free beyond the headroom"
            ),
            "{why}"
        ),
        _ => panic!("refused"),
    }
    assert!(f.state.runtime().list().is_empty());
}

/// Item 3 of the second review: below its on-card part, a pipeline goes
/// ahead beside lmgw's other models only with at least the budget it has
/// rendered with; its measured context is no such budget. With none
/// learned, or less than it, only when nothing else of lmgw's holds the
/// card. Otherwise its start is refused (a Start never evicts) with why, and
/// a request's start evicts what is idle first.
#[tokio::test]
async fn below_its_on_card_part_a_working_budget_decides() {
    // Four starts: one container each.
    let f = fixture_n(4 * GIB, 3 * GIB, GIB, 512, 1).await;
    let id = add_pipeline(
        &f,
        "huge",
        [4 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true}),
        0,
    )
    .await;

    // Nothing else on the card, nothing measured: all that is free.
    let out = start(&f, "huge").await;
    let w = out["warnings"][0].as_str().unwrap_or_default();
    assert!(
        w.contains("gives it 3.5 GiB, all that is free beyond the headroom: short of the 5.0 GiB")
            && w.contains("nothing else of lmgw's on the card to wait for")
            && w.contains("how little it can run with is not measured yet"),
        "{out}"
    );
    assert_eq!(max_vram(&f, "huge"), (Some("2.500".into()), 1));
    stop(&f, "huge").await;

    // The chat model holds 3 GiB: with nothing measured, the Start waits
    // for nothing and is refused, saying why.
    drop(super::image_pipeline_peak::acquire_model(&f, Class::Chat, "chat-model").await);
    let refused = |want: &'static [&'static str]| {
        let f = &f;
        async move {
            let snap = f.state.snapshot();
            match f
                .state
                .vram
                .check_background_start(&f.state, &snap, Class::Image, "huge")
                .await
            {
                Fit::Full(why) => assert!(want.iter().all(|w| why.contains(w)), "{why}"),
                _ => panic!("refused: {want:?}"),
            }
        }
    };
    refused(&[
        "only 1.0 GiB is free",
        "how little is not measured yet",
        "lmgw's other models hold the card",
    ])
    .await;

    // A measured context of 5 MiB (a GTX 970 test card's under Vulkan) is no working
    // budget: still refused, not held to 0.495 GiB beside the chat model.
    learn(&f, id, Some(5 * MIB), None).await;
    refused(&["how little is not measured yet"]).await;

    // A learned working budget of 256 MiB: the 507 MiB the 512 MiB beyond
    // the headroom leaves will do, beside the chat model.
    set_learned(&f, id, store::LearnedFigure::Work, Some(256 * MIB)).await;
    let out = start(&f, "huge").await;
    assert_eq!(out["ok"], true, "{out}");
    let w = out["warnings"][0].as_str().unwrap_or_default();
    assert!(
        w.contains("gives it 512 MiB, all that is free beyond the headroom")
            && w.contains("a budget of at least the 256 MiB it has rendered with")
            && w.contains("its measured 5.0 MiB context outside that budget"),
        "{out}"
    );
    assert_eq!(max_vram(&f, "huge"), (Some("0.495".into()), 1));
    assert!(!f.stops().contains(&"chat-model".to_string()));
    stop(&f, "huge").await;

    // A working budget above what that leaves: refused, naming it.
    set_learned(&f, id, store::LearnedFigure::Work, Some(768 * MIB)).await;
    refused(&["leaves a budget of 507 MiB, less than the 768 MiB it has rendered with"]).await;

    // A request's start evicts the idle chat model first, then goes ahead
    // with what is free.
    mock_images(&f).await;
    let resp = draw(&f, "huge").await;
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    assert!(
        f.stops().contains(&"chat-model".to_string()),
        "the idle chat model made room: {:?}",
        f.stops()
    );
}

/// The working budget is learned: a generation that completes on an entry
/// held to its grant teaches the row that budget, and a smaller one lowers
/// it; an entry the row's own `max_vram` holds teaches nothing.
#[tokio::test]
async fn a_completed_generation_teaches_the_working_budget() {
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
    mock_images(&f).await;
    let resp = draw(&f, "turbo").await;
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    assert_eq!(
        work(),
        Some(5 * GIB / 2),
        "the 2.5 GiB budget it rendered with"
    );

    // A larger budget teaches nothing; the figure only goes down.
    set_learned(&f, id, store::LearnedFigure::Work, Some(GIB)).await;
    assert_eq!(draw(&f, "turbo").await.status(), 200);
    assert_eq!(work(), Some(GIB));

    // Held by its own max_vram: nothing learned.
    set_learned(&f, id, store::LearnedFigure::Work, None).await;
    let patch: lmgw_core::ops::ImageModelPatch = serde_json::from_value(json!({
        "action": "update",
        "id": id,
        "args": {"offload_to_cpu": true, "max_vram": "1"},
    }))
    .unwrap();
    lmgw_core::ops::image_model_set(&f.state, patch)
        .await
        .unwrap();
    assert_eq!(draw(&f, "turbo").await.status(), 200);
    assert_eq!(work(), None);
}

/// Item 1 of the second review: a whole start of a row whose weights all
/// wait off the card, with a learned peak and no context known, is charged
/// no less than before it adapted — its files and its peak — and with what
/// its processes hold read, that and its peak.
#[tokio::test]
async fn a_learned_peak_without_a_context_is_never_charged_less() {
    let f = fixture(16 * GIB, GIB, GIB, 512).await;
    let id = add_pipeline(
        &f,
        "turbo",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true}),
        768 * MIB,
    )
    .await;
    learn(&f, id, None, Some(7 * GIB)).await;
    assert_eq!(start(&f, "turbo").await["ok"], true);

    // No per-process figures: its files and its peak, all kept free.
    let v = vram_status(&f.gateway).await;
    let r = image_resident(&v);
    assert_eq!(r["estimated_bytes"], 12 * GIB + 512 * MIB, "{r}");
    assert_eq!(
        v["free_bytes"],
        16 * GIB - 768 * MIB - (12 * GIB + 512 * MIB),
        "{v}"
    );

    // Budget-only: the same estimate.
    let mut s = f.state.snapshot().settings.clone();
    s.vram.budget_mb = 16 * 1024;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    f.state
        .vram
        .set_probe(Arc::new(lmgw_core::vram::nvml::NoTelemetry(
            "no driver on this host".into(),
        )));
    let v = vram_status(&f.gateway).await;
    assert_eq!(v["estimated_resident_bytes"], 12 * GIB + 512 * MIB, "{v}");
    assert_eq!(v["free_bytes"], 16 * GIB - (12 * GIB + 512 * MIB), "{v}");
}

/// Item 1, measured: what its processes hold now stands for the context
/// until one is learned (and the context once it is), and only the peak is
/// kept free on top.
#[tokio::test]
async fn a_learned_peak_with_a_reading_is_charged_peak_and_reading() {
    let f = fixture(16 * GIB, GIB, GIB, 512).await;
    let id = add_pipeline(
        &f,
        "turbo",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true}),
        768 * MIB,
    )
    .await;
    learn(&f, id, None, Some(7 * GIB)).await;
    f.attribute(0);
    assert_eq!(start(&f, "turbo").await["ok"], true);
    // Read now, or learned as its context by now: the same charge.
    let v = vram_status(&f.gateway).await;
    let r = image_resident(&v);
    assert_eq!(r["estimated_bytes"], 7 * GIB + 768 * MIB, "{r}");
    assert_eq!(v["free_bytes"], 16 * GIB - 768 * MIB - 7 * GIB, "{v}");

    // Third review: a reading while a generation runs holds the peak too and
    // is no context. With none measured (cleared once read at rest), an
    // entry in flight is charged its files and its peak, not its peak and
    // what it draws mid-generation.
    for _ in 0..200 {
        let learned = f
            .state
            .snapshot()
            .image_models
            .iter()
            .any(|r| r.id == id && r.idle_bytes.is_some());
        if learned {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    set_learned(&f, id, store::LearnedFigure::Idle, None).await;
    let claim = super::image_pipeline_peak::acquire_image(&f, "turbo").await;
    f.world().size.insert("turbo".into(), 7 * GIB + 768 * MIB);
    let v = vram_status(&f.gateway).await;
    let r = image_resident(&v);
    assert_eq!(
        r["estimated_bytes"],
        5 * GIB + 512 * MIB + 7 * GIB,
        "files and peak, not peak and the mid-generation reading: {r}"
    );
    drop(claim);
}

/// Fix 4: sd.cpp's auto-fit is on unless the row turns it off, so a row with
/// no offload setting at all runs with less too; so does a per-module
/// `params_backend`. Only `auto_fit off` (or a GPU-only `params_backend`)
/// pins the weights, and that row is refused with what would let it run.
#[tokio::test]
async fn auto_fit_and_per_module_rows_adapt_and_only_pinned_rows_are_refused() {
    let f = fixture(4 * GIB, GIB, GIB, 512).await;
    for (id, args) in [
        ("auto", json!({})),
        (
            "modules",
            json!({"params_backend": "diffusion=disk,llm=cpu"}),
        ),
    ] {
        add_pipeline(&f, id, [3 * GIB, 2 * GIB, 512 * MIB], args, 0).await;
        let out = start(&f, id).await;
        assert_eq!(out["ok"], true, "{id}: {out}");
        let w = out["warnings"][0].as_str().unwrap_or_default();
        assert!(
            w.starts_with(LEAD) && w.contains("gives it 3.5 GiB, all that is free"),
            "{id}: {out}"
        );
        assert_eq!(max_vram(&f, id), (Some("2.500".into()), 1), "{id}");
        stop(&f, id).await;
    }
    let got = crate::common::read_wire(
        lmgw_core::ops::local_model_get(&f.state, None, Some("auto"), Some("image"))
            .await
            .unwrap(),
    );
    assert!(
        got["warnings"][0]
            .as_str()
            .is_some_and(|w| w.contains("sd.cpp's auto-fit (on unless auto_fit off)")),
        "{got}"
    );

    add_pipeline(
        &f,
        "pinned",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"auto_fit": "off"}),
        0,
    )
    .await;
    let body: Value = draw(&f, "pinned").await.json().await.unwrap();
    assert_eq!(body["error"]["code"], "vram_too_large", "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("needs 5.5 GiB of GPU memory plus 512 MiB headroom")
            && msg.contains("the GPU holds 4.0 GiB in total")
            && msg.contains("auto_fit off keeps all of its weights on the card")
            && msg.contains("offload_to_cpu, a params_backend of cpu or disk"),
        "{msg}"
    );
    add_pipeline(
        &f,
        "gpu-only",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"params_backend": "cuda0"}),
        0,
    )
    .await;
    let snap = f.state.snapshot();
    match f
        .state
        .vram
        .check_background_start(&f.state, &snap, Class::Image, "gpu-only")
        .await
    {
        Fit::Full(why) => assert!(
            why.contains("params_backend cuda0 (which turns sd.cpp's auto-fit off) keeps all"),
            "{why}"
        ),
        _ => panic!("a pipeline pinned on the card that is larger than it is refused"),
    }
    assert!(
        f.runs().iter().all(|m| m != "pinned" && m != "gpu-only"),
        "{:?}",
        f.runs()
    );
}

/// Fix 7: the grant is taken by the entry its start creates and kept for the
/// entry's life — its warning from the claim on, through `ready`, its
/// `--max-vram` in the launch — whatever happens to the reservation that
/// staged it. Staged by hand here, with admission off, so nothing else
/// reserves or stages.
#[tokio::test]
async fn the_entry_keeps_its_grant_from_claim_to_ready() {
    let f = fixture(4 * GIB, GIB, GIB, 512).await;
    add_pipeline(
        &f,
        "turbo",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true}),
        0,
    )
    .await;
    let mut s = f.state.snapshot().settings.clone();
    s.vram.enabled = false;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();

    let warning = format!("{LEAD} image/turbo, by hand");
    f.state.runtime().stage_grant(
        Class::Image,
        "turbo",
        7,
        lmgw_core::runtime::registry::Staged {
            grant: Some(PartialGrant {
                bytes: 2 * GIB,
                max_vram_bytes: 3 * GIB / 2,
                warning: warning.clone(),
                llama: None,
            }),
            note: None,
        },
    );
    // Another reservation's drop does not take it away.
    f.state.runtime().unstage_grant(Class::Image, "turbo", 8);
    let out = start(&f, "turbo").await;
    assert_eq!(out["ok"], true, "{out}");
    assert!(entry_warnings(&f, "turbo").contains(&warning));
    assert_eq!(max_vram(&f, "turbo"), (Some("1.500".into()), 1));
    let grant = f
        .state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == "turbo")
        .and_then(|v| v.grant);
    assert_eq!(grant.map(|g| g.bytes), Some(2 * GIB));

    // Taken: the next start of the model, with nothing staged, has none.
    stop(&f, "turbo").await;
    let out = start(&f, "turbo").await;
    assert_eq!(out["ok"], true, "{out}");
    assert!(!entry_warnings(&f, "turbo").contains(&warning));
    assert_eq!(max_vram(&f, "turbo"), (None, 0));
}

/// Fix 3's floor is a measured figure: a ready container of a row whose
/// weights all wait off the card is read once at rest, per process, and what
/// its processes hold becomes the row's idle residency. Nothing is read for
/// an auto-fit row, whose idle residency holds placed weights; a `files` or
/// `args` change resets it.
#[tokio::test]
async fn the_floor_is_read_per_process_at_rest() {
    let f = fixture(16 * GIB, GIB, GIB, 512).await;
    f.attribute(0);
    let id = add_pipeline(
        &f,
        "turbo",
        [3 * GIB, 2 * GIB, 512 * MIB],
        json!({"offload_to_cpu": true}),
        768 * MIB,
    )
    .await;
    add_pipeline(&f, "auto", [GIB, GIB, GIB], json!({}), 512 * MIB).await;
    for m in ["turbo", "auto"] {
        assert_eq!(start(&f, m).await["ok"], true);
    }
    let idle = |m: &str| {
        f.state
            .snapshot()
            .image_models
            .iter()
            .find(|r| r.model_id == m)
            .and_then(|r| r.idle_bytes)
    };
    for _ in 0..200 {
        if idle("turbo").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(idle("turbo"), Some(768 * MIB), "its context, read at rest");
    assert_eq!(idle("auto"), None, "no floor for an auto-fit row");

    // Its charge is its files, what its processes hold now kept out of what
    // is kept free; the auto-fit row is charged what the driver shows.
    let v = vram_status(&f.gateway).await;
    assert_eq!(
        v["free_bytes"],
        16 * GIB - 768 * MIB - 512 * MIB - (5 * GIB + 512 * MIB - 768 * MIB),
        "{v}"
    );

    // An edit of its args forgets it.
    let patch: lmgw_core::ops::ImageModelPatch = serde_json::from_value(json!({
        "action": "update",
        "id": id,
        "args": {"offload_to_cpu": true, "steps": 4},
    }))
    .unwrap();
    lmgw_core::ops::image_model_set(&f.state, patch)
        .await
        .unwrap();
    assert_eq!(idle("turbo"), None);

    // So does another engine image, with the working budget.
    set_learned(&f, id, store::LearnedFigure::Idle, Some(GIB)).await;
    set_learned(&f, id, store::LearnedFigure::Work, Some(GIB)).await;
    let patch: lmgw_core::ops::ImageModelPatch = serde_json::from_value(json!({
        "action": "update",
        "id": id,
        "image": "registry.example/sd:vulkan",
    }))
    .unwrap();
    lmgw_core::ops::image_model_set(&f.state, patch)
        .await
        .unwrap();
    let row = f
        .state
        .snapshot()
        .image_models
        .iter()
        .find(|r| r.id == id)
        .cloned()
        .unwrap();
    assert_eq!((row.idle_bytes, row.work_bytes), (None, None));
}
