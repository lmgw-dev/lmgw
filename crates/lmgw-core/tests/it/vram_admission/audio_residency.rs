//! The learned residency of an audio model (realtime design §9.4, WP7)
//!
//! audio.cpp loads a model on its first request, and its readiness route
//! answers before that — so an audio container that is up but has never been
//! asked anything holds only its CUDA context, and the driver shows nothing
//! of the weights it is about to load. The fake containers here play that:
//! a container holds [`CONTEXT`] from its start, and the first audio request
//! it answers grows it to the row's `World::loaded_bytes`.

use lmgw_core::config::{AudioModel, LearnedResidency};
use lmgw_core::runtime::Class;
use lmgw_core::store::NewAudioModel;
use lmgw_core::vram::residency::{learned, resident_key};
use lmgw_core::vram::Fit;

use super::*;

const MIB: u64 = 1024 * 1024;

/// What an audio container holds before it has loaded anything.
pub(super) const CONTEXT: u64 = 256 * MIB;

/// An audio row whose model directory is `on_disk` bytes, and whose
/// container holds [`CONTEXT`] until its first request and `loaded` after
/// it. `lazy: None` inherits the class (lazy).
pub(super) async fn add_audio_model(
    f: &Fixture,
    model_id: &str,
    on_disk: u64,
    loaded: u64,
    lazy: Option<bool>,
) -> i64 {
    let dir = f._models_dir.path().join(model_id);
    std::fs::create_dir_all(&dir).unwrap();
    // Sparse: the size is metadata.
    std::fs::File::create(dir.join("model.gguf"))
        .unwrap()
        .set_len(on_disk)
        .unwrap();
    {
        let mut w = f.world();
        w.size.insert(model_id.into(), CONTEXT);
        w.loaded_bytes.insert(model_id.into(), loaded);
    }
    let id = store::insert_audio_model(
        &f.state.db,
        &NewAudioModel {
            model_id: model_id.into(),
            family: "pocket_tts".into(),
            path: model_id.into(),
            task: "tts".into(),
            mode: "offline".into(),
            lazy,
            busy_timeout_ms: None,
            backend: None,
            threads: None,
            load_options: Default::default(),
            session_options: Default::default(),
            default_request_options: Default::default(),
            model_spec_override: None,
            config_id: None,
            weight_id: None,
            voice_presets: Default::default(),
            default_voice_preset: None,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    id
}

/// Bring the container up the way a warm start does — no request, no
/// claim left behind: `ready`, and nothing loaded.
pub(super) async fn start_idle(f: &Fixture, model_id: &str) {
    let snap = f.state.snapshot();
    let rt = lmgw_core::runtime::descriptor::model_runtime(&snap, Class::Audio, model_id)
        .expect("the row was just inserted");
    let spec = lmgw_core::runtime::lifecycle::acquire_spec(&f.state, &snap, &rt);
    drop(f.state.runtime().acquire(&spec).await.unwrap());
}

/// One TTS request through the gateway, read to the end.
pub(super) async fn speak(f: &Fixture, model_id: &str) -> u16 {
    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/audio/speech", f.gateway))
        .json(&json!({"model": format!("audio/{model_id}"), "input": "hello", "voice": "alba"}))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    resp.bytes().await.unwrap();
    status
}

/// Until the residency reading after `before` readings is over.
pub(super) async fn readings_after(f: &Fixture, before: u64) {
    common::patience::until(
        &format!("a residency reading finished after {before}"),
        || f.state.vram.residency_readings() > before,
    )
    .await;
}

/// Until the sampler stretch after `before` stretches is over.
pub(super) async fn stretches_after(f: &Fixture, before: u64) {
    common::patience::until(&format!("a sampler stretch ended after {before}"), || {
        f.state.vram.residency_stretches() > before
    })
    .await;
}

/// [`speak`], and wait for the reading it starts.
pub(super) async fn speak_and_read(f: &Fixture, model_id: &str) {
    let before = f.state.vram.residency_readings();
    assert_eq!(speak(f, model_id).await, 200);
    readings_after(f, before).await;
}

pub(super) fn row(f: &Fixture, model_id: &str) -> AudioModel {
    f.state
        .snapshot()
        .audio_models
        .iter()
        .find(|m| m.model_id == model_id)
        .cloned()
        .unwrap()
}

pub(super) fn residency(f: &Fixture, model_id: &str) -> Option<LearnedResidency> {
    row(f, model_id).residency
}

pub(super) fn learned_now(f: &Fixture, model_id: &str) -> Option<u64> {
    learned(&row(f, model_id), &f.state.snapshot().settings.audio)
}

pub(super) fn audio_resident(v: &Value) -> Value {
    v["resident"]
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["container"] == "audio").cloned())
        .unwrap_or_else(|| panic!("no audio resident in {v}"))
}

pub(super) fn note(r: &Value) -> String {
    r["note"].as_str().unwrap_or_default().to_string()
}

/// The models page's view of one audio row, as the typed DTO reads it.
pub(super) async fn models_page(f: &Fixture, model_id: &str) -> lmgw_api_types::AudioModelView {
    let listed: lmgw_api_types::ModelsFull = f
        .gateway
        .client()
        .get(format!("{}/api/models/full", f.gateway))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    listed
        .audio
        .into_iter()
        .find(|r| r.model.model_id == model_id)
        .unwrap_or_else(|| panic!("no audio row {model_id}"))
}

/// The models page's sentence for one audio row.
pub(super) async fn models_page_note(f: &Fixture, model_id: &str) -> String {
    models_page(f, model_id)
        .await
        .residency_note
        .unwrap_or_else(|| panic!("no residency note for {model_id}"))
}

pub(super) async fn op(f: &Fixture, name: &str, args: Value) -> Value {
    let resp = f
        .gateway
        .client()
        .post(format!("{}/api/op/{name}", f.gateway))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    body
}

/// Store a learned figure for the row's current configuration, as a
/// request would have.
pub(super) async fn learn(f: &Fixture, id: i64, model_id: &str, bytes: u64) {
    let key = resident_key(&row(f, model_id), &f.state.snapshot().settings.audio);
    store::set_audio_model_residency(&f.state.db, id, Some((bytes, &key)))
        .await
        .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// The hole this closes, in the numbers: a warm TTS container that has
/// never been asked anything holds its CUDA context and nothing else, and
/// without the pending charge the ledger offered the 2 GiB it is about to
/// load to the next start. With it, a warm start that fits beside the
/// context but not beside the load is refused, and says why — and a request
/// evicts the idle audio container instead of squeezing in beside it.
#[tokio::test]
async fn a_warm_audio_model_keeps_its_load_free_until_its_first_request() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    start_idle(&f, "tts").await;

    let v = vram_status(&f.gateway).await;
    assert_eq!(v["devices"][0]["used_bytes"], CONTEXT, "only the context");
    assert_eq!(
        v["free_bytes"],
        10 * GIB - CONTEXT - 2 * GIB,
        "the load still to come is off the free figure: {v}"
    );
    let r = audio_resident(&v);
    assert_eq!(r["pending_bytes"], 2 * GIB);
    assert_eq!(r["estimated_bytes"], 2 * GIB, "unlearned: the on-disk size");
    let n = note(&r);
    assert!(
        n.contains("Not loaded yet") && n.contains("kept free until its first request"),
        "{n}"
    );
    assert!(n.contains("residency not learned"), "{n}");

    // 8 GiB fits beside the context (9.75 free) but not beside the load.
    let snap = f.state.snapshot();
    match f
        .state
        .vram
        .check_background_start(&f.state, &snap, Class::Chat, "chat-model")
        .await
    {
        Fit::Full(why) => assert!(
            why.contains("audio/tts") && why.contains("until loaded"),
            "the refusal names what it keeps free: {why}"
        ),
        _ => panic!("a warm start into a pending load has to be refused"),
    }

    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(
        f.stops(),
        vec!["tts".to_string()],
        "the idle audio container is evicted, its pending load with it"
    );
}

/// The first request loads the model and teaches the row what its
/// container holds: from then on that figure — not the files — is what the
/// row is charged, the next cold start included, and nothing is pending.
#[tokio::test]
async fn the_first_request_teaches_the_residency_and_admission_charges_it() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;

    speak_and_read(&f, "tts").await;
    let m = row(&f, "tts");
    let r = m.residency.clone().expect("one request teaches it");
    assert_eq!(r.bytes, 3 * GIB);
    assert_eq!(r.key, resident_key(&m, &f.state.snapshot().settings.audio));
    assert!(!r.learned_at.is_empty());

    let v = vram_status(&f.gateway).await;
    let a = audio_resident(&v);
    assert!(a["pending_bytes"].is_null(), "loaded: nothing pending: {a}");
    assert_eq!(a["estimated_bytes"], 3 * GIB);
    assert_eq!(
        v["free_bytes"],
        7 * GIB,
        "the driver's figure, nothing on top"
    );
    assert!(note(&a).contains("resident 3.0 GiB learned"), "{a}");
    let page = models_page(&f, "tts").await;
    assert!(
        page.residency_note
            .as_deref()
            .unwrap_or_default()
            .contains("resident 3.0 GiB learned"),
        "{page:?}"
    );
    assert_eq!(page.residency_charged_bytes, Some(3 * GIB));
    assert_eq!(page.model.residency.map(|r| r.bytes), Some(3 * GIB));

    // The next cold start is sized by it: 2 GiB free fits the files, not the
    // learned figure.
    f.state
        .runtime()
        .stop(Class::Audio, "tts", false)
        .await
        .unwrap();
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let snap = f.state.snapshot();
    match f
        .state
        .vram
        .check_background_start(&f.state, &snap, Class::Audio, "tts")
        .await
    {
        Fit::Full(why) => assert!(why.contains("'tts' needs 3.0 GiB"), "{why}"),
        _ => panic!("the learned figure has to size the next start"),
    }
}

/// A running maximum: a reading after a smaller request does not lower the
/// charge, a larger one raises it.
#[tokio::test]
async fn a_smaller_reading_never_lowers_the_figure_and_a_larger_one_raises_it() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    speak_and_read(&f, "tts").await;
    assert_eq!(residency(&f, "tts").unwrap().bytes, 3 * GIB);

    f.world().loaded_bytes.insert("tts".into(), 2 * GIB);
    speak_and_read(&f, "tts").await;
    assert_eq!(residency(&f, "tts").unwrap().bytes, 3 * GIB);

    f.world().loaded_bytes.insert("tts".into(), 4 * GIB);
    speak_and_read(&f, "tts").await;
    assert_eq!(residency(&f, "tts").unwrap().bytes, 4 * GIB);
}

/// A figure belongs to the configuration it was read under. A voice preset
/// loads nothing and keeps it; a session option (the quantization) is
/// another model to the card: the figure is kept but not charged, the
/// surfaces say so, and the next request teaches the new configuration. A
/// class image changed under a running container is the same — and that
/// container, still on the previous image, teaches the row nothing.
#[tokio::test]
async fn a_figure_learned_for_another_configuration_is_kept_but_not_charged() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    speak_and_read(&f, "tts").await;
    let first = residency(&f, "tts").unwrap();

    op(
        &f,
        "audio_model_set",
        json!({"action": "update", "id": id,
               "voice_presets": {"alba": {"voice_id": "alba"}}}),
    )
    .await;
    assert_eq!(
        learned_now(&f, "tts"),
        Some(3 * GIB),
        "a voice loads nothing"
    );

    op(
        &f,
        "audio_model_set",
        json!({"action": "update", "id": id,
               "session_options": {"weight_type": "q4_0"}}),
    )
    .await;
    assert_eq!(residency(&f, "tts"), Some(first.clone()), "kept");
    assert_eq!(learned_now(&f, "tts"), None, "but not charged");
    let page = models_page(&f, "tts").await;
    assert_eq!(page.residency_charged_bytes, None, "not charged");
    assert_eq!(page.model.residency.map(|r| r.bytes), Some(3 * GIB), "kept");
    let n = page.residency_note.unwrap_or_default();
    assert!(
        n.contains("learned for a previous configuration") && n.contains("on-disk 2.0 GiB"),
        "{n}"
    );
    start_idle(&f, "tts").await;
    let v = vram_status(&f.gateway).await;
    let a = audio_resident(&v);
    assert_eq!(a["estimated_bytes"], 2 * GIB, "the files again: {a}");
    assert!(note(&a).contains("previous configuration"), "{a}");

    // The new configuration's first request teaches it.
    speak_and_read(&f, "tts").await;
    let second = residency(&f, "tts").unwrap();
    assert_ne!(second.key, first.key);
    assert_eq!(learned_now(&f, "tts"), Some(3 * GIB));

    // The class image changes; the running container keeps the old one.
    let mut s = f.state.snapshot().settings.clone();
    s.audio.image = "example.org/audio.cpp:other".into();
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    assert_eq!(learned_now(&f, "tts"), None);
    f.world().loaded_bytes.insert("tts".into(), 5 * GIB);
    speak_and_read(&f, "tts").await;
    assert_eq!(
        residency(&f, "tts"),
        Some(second),
        "a container still running the previous image teaches the new one nothing"
    );
    assert!(models_page_note(&f, "tts")
        .await
        .contains("learned for a previous configuration"));
}

/// No per-process figures, nothing learned — and no device-wide guess in
/// their place. The load is still known from the answer, so the pending
/// charge goes; the surfaces say why the figure is the files.
#[tokio::test]
async fn without_per_process_figures_nothing_is_learned_and_the_load_still_counts() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    start_idle(&f, "tts").await;
    assert_eq!(
        audio_resident(&vram_status(&f.gateway).await)["pending_bytes"],
        2 * GIB
    );

    let before = f.state.vram.residency_readings();
    assert_eq!(speak(&f, "tts").await, 200);
    let v = vram_status(&f.gateway).await;
    let a = audio_resident(&v);
    assert!(a["pending_bytes"].is_null(), "{a}");
    assert_eq!(v["devices"][0]["used_bytes"], 3 * GIB);
    assert_eq!(v["free_bytes"], 7 * GIB);
    assert!(
        note(&a).contains("cannot learn: FakeGPU lists no processes"),
        "{a}"
    );
    assert_eq!(residency(&f, "tts"), None);
    assert_eq!(
        f.state.vram.residency_readings(),
        before,
        "nothing was read, so nothing could be stored"
    );
    assert!(models_page_note(&f, "tts").await.contains("cannot learn"));
}

/// A declared budget on a box with no telemetry plans against estimates, and
/// the estimate of a learned row is its learned figure. Nothing is pending on
/// top: in that branch the whole expected residency is already counted.
#[tokio::test]
async fn the_budget_only_branch_charges_the_learned_figure() {
    let f = fixture(20 * GIB, 6 * GIB, 3 * GIB, 0).await;
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    learn(&f, id, "tts", 3 * GIB).await;
    let mut s = f.state.snapshot().settings.clone();
    s.vram.budget_mb = 20 * 1024;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    f.state
        .vram
        .set_probe(Arc::new(lmgw_core::vram::nvml::NoTelemetry(
            "no driver on this host".into(),
        )));
    start_idle(&f, "tts").await;

    let v = vram_status(&f.gateway).await;
    assert_eq!(v["active"], true);
    assert_eq!(v["free_measured"], false);
    assert_eq!(v["estimated_resident_bytes"], 3 * GIB);
    assert_eq!(v["free_bytes"], 17 * GIB, "charged once: {v}");
}

/// An eager row has its weights on the card once it is ready — the driver
/// shows them — so only what the first request adds on top is pending.
#[tokio::test]
async fn an_eager_row_keeps_free_only_what_comes_on_top_of_its_weights() {
    let f = fixture(10 * GIB, 6 * GIB, GIB, 0).await;
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, Some(false)).await;
    learn(&f, id, "tts", 3 * GIB).await;
    f.world().size.insert("tts".into(), 2 * GIB);
    start_idle(&f, "tts").await;

    let v = vram_status(&f.gateway).await;
    let a = audio_resident(&v);
    assert_eq!(a["pending_bytes"], GIB, "{a}");
    assert_eq!(v["free_bytes"], 10 * GIB - 2 * GIB - GIB);
    assert!(note(&a).contains("1.0 GiB more kept free"), "{a}");
}

/// The owner's reset: back to the files until a request teaches it again,
/// and the answer says so.
#[tokio::test]
async fn the_owner_can_reset_a_learned_figure() {
    let f = fixture(10 * GIB, 6 * GIB, GIB, 0).await;
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    learn(&f, id, "tts", 3 * GIB).await;

    let out = op(
        &f,
        "audio_model_set",
        json!({"action": "update", "id": id, "clear": "residency"}),
    )
    .await;
    assert_eq!(out["residency_reset"], true, "{out}");
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("learned residency was reset"),
        "{out}"
    );
    assert_eq!(residency(&f, "tts"), None);

    let out = op(
        &f,
        "audio_model_set",
        json!({"action": "update", "id": id, "clear": "residency"}),
    )
    .await;
    assert_eq!(out["residency_reset"], false, "nothing to reset: {out}");
}
