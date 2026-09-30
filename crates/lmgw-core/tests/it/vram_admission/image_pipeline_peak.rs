//! The learned transient peak of an image pipeline (image-generation §9)

use super::*;

/// An sd-server pipeline in the fixture's world: a sparse file of `bytes` in
/// the shared models dir, an enabled row naming it, and an entry in the
/// driver's book so its container moves the free figure like every other
/// model's does.
///
/// `bytes` is the pipeline's **idle** residency — the only part of it a driver
/// reading an idle card can see. What one generation needs on top is the thing
/// this section is about, and it is not in any file size.
pub(super) async fn add_image_model(f: &Fixture, model_id: &str, bytes: u64) -> i64 {
    let file = format!("{model_id}.gguf");
    let path = f._models_dir.path().join(&file);
    std::fs::File::create(&path)
        .unwrap()
        .set_len(bytes)
        .unwrap();
    f.world().size.insert(model_id.to_string(), bytes);

    let mut files = serde_json::Map::new();
    files.insert("diffusion_model".into(), json!(file));
    let id = store::insert_image_model(
        &f.state.db,
        &store::NewImageModel {
            model_id: model_id.to_string(),
            files,
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

/// Bring a model up the way a request does — through the registry's own
/// `acquire`, which is what hands out the in-flight claim the sampler reads.
/// The returned guard *is* the in-flight window; drop it to close it.
async fn acquire_model(
    f: &Fixture,
    class: lmgw_core::runtime::Class,
    model_id: &str,
) -> lmgw_core::runtime::registry::AcquireGuard {
    let snap = f.state.snapshot();
    let rt = lmgw_core::runtime::descriptor::model_runtime(&snap, class, model_id)
        .expect("the row was just inserted");
    let spec = lmgw_core::runtime::lifecycle::acquire_spec(&f.state.data_dir, &snap, &rt);
    f.state.runtime().acquire(&spec).await.unwrap()
}

async fn acquire_image(f: &Fixture, model_id: &str) -> lmgw_core::runtime::registry::AcquireGuard {
    acquire_model(f, lmgw_core::runtime::Class::Image, model_id).await
}

async fn learn_peak(f: &Fixture, id: i64, bytes: u64) {
    store::set_image_model_peak(&f.state.db, id, Some(bytes))
        .await
        .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

fn image_resident(v: &Value) -> Value {
    v["resident"]
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["container"] == "image").cloned())
        .unwrap_or_else(|| panic!("no image resident in {v}"))
}

/// The gap this whole mechanism exists for, in the numbers: a pipeline that is
/// 6 GiB resident while idle needs 6 more the moment it is asked to draw, and a
/// driver reading an idle card cannot see the second 6. Once one generation has
/// taught lmgw that figure, admission keeps it free — so a chat model that used
/// to fit beside the idle pipeline no longer does.
///
/// Nothing is evictable here (the pipeline has a request in flight), so the
/// answer is the named refusal rather than an eviction, and it has to say what
/// is holding the memory: the idle figure *and* the peak, because they are two
/// different things for an owner to act on.
#[tokio::test]
async fn a_learned_peak_is_kept_free_and_the_refusal_names_it() {
    let f = fixture(20 * GIB, 6 * GIB, 3 * GIB, 0).await;
    let id = add_image_model(&f, "z-image", 6 * GIB).await;
    let guard = acquire_image(&f, "z-image").await;

    // Before anything has been learned: 6 GiB of 20 are held, the chat model
    // needs 6, and it fits with room to spare.
    let v = vram_status(&f.gateway).await;
    assert_eq!(v["devices"][0]["used_bytes"], 6 * GIB);
    assert_eq!(v["free_bytes"], 14 * GIB);
    assert!(image_resident(&v)["peak_extra_bytes"].is_null());

    learn_peak(&f, id, 12 * GIB).await;

    let v = vram_status(&f.gateway).await;
    assert_eq!(
        v["free_bytes"],
        2 * GIB,
        "the learned peak comes off the measured free figure: {v}"
    );
    assert_eq!(
        v["devices"][0]["used_bytes"],
        6 * GIB,
        "and it is not pretended to be on the card — the driver still says 6"
    );
    assert_eq!(
        v["estimated_resident_bytes"],
        6 * GIB,
        "what is resident is what is resident; the peak is a reservation, not a residency"
    );
    let r = image_resident(&v);
    assert_eq!(r["peak_extra_bytes"], 12 * GIB);
    assert!(
        r["note"]
            .as_str()
            .unwrap_or_default()
            .contains("charged at admission"),
        "the resident has to say the peak is being charged: {r}"
    );

    let resp = chat(&f.gateway).await;
    assert_eq!(resp.status(), 503, "at capacity, not a client error");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "vram_queue_timeout");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("image/z-image") && msg.contains("peak"),
        "the refusal has to name the pipeline and the peak it is keeping free: {body}"
    );
    assert!(
        f.stops().is_empty(),
        "a pipeline with a request in flight is never the victim: {:?}",
        f.stops()
    );

    // A background start — a warm start, or the operator's Start button — is
    // the same ledger read the same way, with no special case for this charge:
    // it is refused, and the refusal carries the same sentence.
    let snap = f.state.snapshot();
    let fit = f
        .state
        .vram
        .check_background_start(
            &f.state,
            &snap,
            lmgw_core::runtime::Class::Chat,
            "chat-model",
        )
        .await;
    match fit {
        lmgw_core::vram::Fit::Full(why) => assert!(
            why.contains("image/z-image") && why.contains("peak"),
            "{why}"
        ),
        _ => panic!("a warm start into a peak that is charged has to be refused"),
    }
    drop(guard);
}

/// The same card, the same figures, and the pipeline *idle*: now the charge
/// has somewhere to give. Admission evicts the image model — which frees its
/// idle residency and its peak together, because the ledger simply recomputes
/// without it — and the chat request is answered instead of refused.
///
/// The control is the same run without a learned peak: there the chat model
/// fits beside the idle pipeline and nothing is stopped at all.
#[tokio::test]
async fn an_idle_pipeline_with_a_learned_peak_is_evicted_instead_of_squeezed() {
    for learned in [false, true] {
        let f = fixture(20 * GIB, 6 * GIB, 3 * GIB, 0).await;
        let id = add_image_model(&f, "z-image", 6 * GIB).await;
        drop(acquire_image(&f, "z-image").await);
        if learned {
            learn_peak(&f, id, 12 * GIB).await;
        }

        assert_eq!(chat(&f.gateway).await.status(), 200);
        if learned {
            assert_eq!(
                f.stops(),
                vec!["z-image".to_string()],
                "6 GiB idle + 12 GiB peak leaves 2 GiB: the pipeline has to go"
            );
            let v = vram_status(&f.gateway).await;
            assert!(
                v["resident"]
                    .as_array()
                    .is_none_or(|rs| rs.iter().all(|r| r["container"] != "image")),
                "and the whole charge goes with it: {v}"
            );
            assert_eq!(v["free_bytes"], 14 * GIB);
        } else {
            assert!(
                f.stops().is_empty(),
                "unlearned, the idle pipeline is 6 GiB and the chat model fits beside it: {:?}",
                f.stops()
            );
        }
    }
}

/// What a pipeline that has never generated says about itself. The hole is
/// real — admission is charging nothing for buffers that will be allocated —
/// so it is stated on the resident in words, with the one action that closes
/// it. A silent `null` would make a card that is about to OOM look exactly
/// like one that is safe.
#[tokio::test]
async fn an_unlearned_peak_charges_nothing_and_says_so_on_the_resident() {
    let f = fixture(20 * GIB, 6 * GIB, 3 * GIB, 0).await;
    add_image_model(&f, "z-image", 6 * GIB).await;
    drop(acquire_image(&f, "z-image").await);

    let v = vram_status(&f.gateway).await;
    assert_eq!(v["free_bytes"], 14 * GIB, "nothing unmeasured is charged");
    let r = image_resident(&v);
    assert!(r["peak_extra_bytes"].is_null());
    let note = r["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("peak not learned yet")
            && note.contains("run one generation at the largest size you use"),
        "the gap has to be visible, and say what closes it: {note}"
    );

    // And admission behaves exactly as it did before this mechanism existed.
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert!(f.stops().is_empty(), "{:?}", f.stops());
}

/// The budget-only branch — a declared `vram.budget_mb` on a box with no
/// telemetry at all — charges the peak too. It has to: that branch plans
/// against estimates, and an estimate of an image pipeline that stops at the
/// file sizes is exactly the blind spot the peak was measured to fill.
///
/// (Learned on another box, or before the driver went away: the figure is in
/// the row, and the row is what the ledger reads.)
#[tokio::test]
async fn the_budget_only_branch_charges_the_learned_peak_as_well() {
    let f = fixture(20 * GIB, 6 * GIB, 3 * GIB, 0).await;
    let id = add_image_model(&f, "z-image", 6 * GIB).await;
    drop(acquire_image(&f, "z-image").await);

    let mut s = f.state.snapshot().settings.clone();
    s.vram.budget_mb = 20 * 1024;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    f.state
        .vram
        .set_probe(Arc::new(lmgw_core::vram::nvml::NoTelemetry(
            "no driver on this host".into(),
        )));

    let v = vram_status(&f.gateway).await;
    assert_eq!(v["active"], true, "a budget is something to admit against");
    assert_eq!(v["free_measured"], false);
    assert_eq!(v["free_bytes"], 14 * GIB, "the estimate alone: {v}");

    learn_peak(&f, id, 12 * GIB).await;
    let v = vram_status(&f.gateway).await;
    assert_eq!(
        v["estimated_resident_bytes"],
        6 * GIB,
        "the pipeline's residency is still its files"
    );
    assert_eq!(
        v["free_bytes"],
        2 * GIB,
        "and the peak is subtracted from the declared budget on top: {v}"
    );
    assert_eq!(image_resident(&v)["peak_extra_bytes"], 12 * GIB);
}

/// A driver the test writes by hand, one reading at a time. The sampler reads
/// nothing else — no per-process figures exist on any of these probes — so
/// this is exactly as much as it can ever know.
struct ScriptedGpu {
    total: u64,
    used: Arc<Mutex<u64>>,
}

impl GpuProbe for ScriptedGpu {
    fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        let used = *self.used.lock().unwrap();
        Ok(vec![GpuMemory {
            index: 0,
            name: "ScriptedGPU".into(),
            total_bytes: self.total,
            used_bytes: used,
            free_bytes: self.total.saturating_sub(used),
        }])
    }

    fn source(&self) -> String {
        "ScriptedGPU".into()
    }
}

async fn peak_of(f: &Fixture, model_id: &str) -> Option<u64> {
    f.state
        .snapshot()
        .image_models
        .iter()
        .find(|m| m.model_id == model_id)
        .and_then(|m| m.peak_extra_bytes)
}

/// The sampler, end to end, on a scripted card: idle, then a generation that
/// takes 6 GiB more, then idle again — and the row comes out knowing what that
/// generation cost.
///
/// The three properties that make the figure usable, all in one run:
///
/// * it is the *delta* above the idle reading, not the peak reading, because
///   the idle part is already inside the free memory admission measures;
/// * it is **monotonic** — a later, smaller generation does not lower it, since
///   the charge has to cover the largest job this pipeline has been asked for;
/// * a window that opens with no idle reading *behind* it — the first request
///   after a start, whose guard is taken the moment the container answers its
///   readiness probe — is measured against the first idle reading after it
///   instead, because the buffers are freed when the job ends and what is left
///   is the residency. Otherwise a pipeline that is started per request and
///   reaped after it could never learn anything at all.
#[tokio::test]
async fn the_sampler_learns_what_one_generation_cost_and_never_lowers_it() {
    use lmgw_core::vram::peak::{PeakSampler, IDLE_INTERVAL, IN_FLIGHT_INTERVAL};

    const IDLE: u64 = 7 * GIB;
    const BIG: u64 = 6 * GIB;
    const SMALL: u64 = 2 * GIB;

    let f = fixture(24 * GIB, 6 * GIB, 3 * GIB, 0).await;
    add_image_model(&f, "z-image", IDLE).await;
    let used = Arc::new(Mutex::new(0u64));
    f.state.vram.set_probe(Arc::new(ScriptedGpu {
        total: 24 * GIB,
        used: used.clone(),
    }));
    let set = |bytes: u64| *used.lock().unwrap() = bytes;
    let sampler = PeakSampler::new();

    // Nothing is up: the registry is read, the driver is not, and the next
    // pass is the slow one.
    assert_eq!(sampler.tick(&f.state).await, IDLE_INTERVAL);

    // The first request after the start: its guard is taken the moment the
    // container is ready, so this window has no idle reading behind it — and
    // is measured against the one that follows it.
    set(IDLE);
    let guard = acquire_image(&f, "z-image").await;
    assert_eq!(
        sampler.tick(&f.state).await,
        IN_FLIGHT_INTERVAL,
        "a pass that saw a generation running asks for the fast rate"
    );
    set(IDLE + SMALL);
    sampler.tick(&f.state).await;
    drop(guard);
    set(IDLE);
    sampler.tick(&f.state).await;
    assert_eq!(
        peak_of(&f, "z-image").await,
        Some(SMALL),
        "a first window is measured against the idle reading after it"
    );

    // That last pass also took a baseline the normal way.
    let guard = acquire_image(&f, "z-image").await;
    sampler.tick(&f.state).await;
    set(IDLE + BIG);
    sampler.tick(&f.state).await;
    // The plateau holds for several samples and then the buffers are freed —
    // the max is what is kept, not the last reading.
    set(IDLE + BIG - GIB);
    sampler.tick(&f.state).await;
    drop(guard);
    set(IDLE);
    sampler.tick(&f.state).await;
    assert_eq!(
        peak_of(&f, "z-image").await,
        Some(BIG),
        "the delta above idle, and the largest one in the window"
    );

    // A smaller generation afterwards: the same pipeline, a 256² job instead
    // of a 1024² one. It must not lower the charge — including through the
    // path above, which is why the first window used the small figure.
    let guard = acquire_image(&f, "z-image").await;
    sampler.tick(&f.state).await;
    set(IDLE + SMALL);
    sampler.tick(&f.state).await;
    drop(guard);
    set(IDLE);
    sampler.tick(&f.state).await;
    assert_eq!(peak_of(&f, "z-image").await, Some(BIG));

    // And a bigger one raises it, which is the whole point of keeping the
    // largest rather than the last.
    let guard = acquire_image(&f, "z-image").await;
    sampler.tick(&f.state).await;
    set(IDLE + BIG + GIB);
    sampler.tick(&f.state).await;
    drop(guard);
    set(IDLE);
    sampler.tick(&f.state).await;
    assert_eq!(peak_of(&f, "z-image").await, Some(BIG + GIB));
}

/// A window during which something else took memory on the same card teaches
/// nothing. NVML answers per device, not per process, so a chat model loading
/// beside a generation is indistinguishable from a generation that allocated
/// its weights — and a figure learned that way would be charged to this row
/// forever, on evidence that was never about it.
#[tokio::test]
async fn a_window_shared_with_another_start_teaches_nothing() {
    use lmgw_core::vram::peak::PeakSampler;

    const IDLE: u64 = 7 * GIB;
    let f = fixture(24 * GIB, 6 * GIB, 3 * GIB, 0).await;
    add_image_model(&f, "z-image", IDLE).await;
    let used = Arc::new(Mutex::new(IDLE));
    f.state.vram.set_probe(Arc::new(ScriptedGpu {
        total: 24 * GIB,
        used: used.clone(),
    }));
    let sampler = PeakSampler::new();

    drop(acquire_image(&f, "z-image").await);
    sampler.tick(&f.state).await; // baseline

    let guard = acquire_image(&f, "z-image").await;
    sampler.tick(&f.state).await;
    // The chat model's container comes up mid-generation and its 6 GiB land in
    // the same `used` figure.
    let chat_guard = acquire_model(&f, lmgw_core::runtime::Class::Chat, "chat-model").await;
    *used.lock().unwrap() = IDLE + 6 * GIB;
    sampler.tick(&f.state).await;
    drop(guard);
    sampler.tick(&f.state).await;

    assert_eq!(
        peak_of(&f, "z-image").await,
        None,
        "the rise belongs to the other container, so this row learns nothing"
    );
    drop(chat_guard);
}

/// A learned peak describes the pipeline it was measured on. Change the files
/// or one flag — `offload_to_cpu` alone moves the measured peak from 13.7 GiB
/// to 7.7 — and the measurement is about something that no longer exists, so
/// it is dropped and the response says so. A change that touches neither keeps
/// it: re-learning costs a generation, and there is nothing to re-learn.
#[tokio::test]
async fn changing_the_pipeline_resets_its_learned_peak_and_says_so() {
    let f = fixture(24 * GIB, 6 * GIB, 3 * GIB, 0).await;
    let id = add_image_model(&f, "z-image", 7 * GIB).await;
    learn_peak(&f, id, 6 * GIB).await;

    // Something that is not the pipeline: the measurement still holds.
    let patch = |v: Value| serde_json::from_value::<lmgw_core::ops::ImageModelPatch>(v).unwrap();
    let out = lmgw_core::ops::image_model_set(
        &f.state,
        patch(json!({"action": "update", "id": id, "idle_seconds": 60})),
    )
    .await
    .unwrap();
    assert_eq!(out["peak_reset"], false, "{out}");
    assert_eq!(peak_of(&f, "z-image").await, Some(6 * GIB));

    // One flag, and the pipeline is a different one to the GPU.
    let out = lmgw_core::ops::image_model_set(
        &f.state,
        patch(json!({"action": "update", "id": id, "args": {"offload_to_cpu": true}})),
    )
    .await
    .unwrap();
    assert_eq!(out["peak_reset"], true, "{out}");
    let msg = out["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("learned peak reset"),
        "a promise admission has stopped making is not a silent change: {msg}"
    );
    assert_eq!(peak_of(&f, "z-image").await, None);

    // And the surface an agent reads says what to do about it.
    let got = lmgw_core::ops::local_model_get(&f.state, None, Some("z-image"), Some("image"))
        .await
        .unwrap();
    assert!(got["peak_extra_bytes"].is_null());
    assert!(
        got["peak"]
            .as_str()
            .unwrap_or_default()
            .contains("not learned yet"),
        "{got}"
    );
}
