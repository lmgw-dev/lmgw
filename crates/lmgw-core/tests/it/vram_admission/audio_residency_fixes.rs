//! The WP7 review's fixes to the learned audio residency (fix package B7):
//! a streamed answer counts once its relay ends (M1), the image peak window
//! opens only against a baseline that saw what it sees (M2), an unloaded
//! model's reload is pending while it runs (M3), a ready container is read
//! at rest so its context is not counted twice (M4) — and the lows: a failed
//! reading says why and is retried, a reset drops a reading in flight,
//! tombstones settle with the trigger off, an eager row subtracts the weights
//! file it loads, and a container still running a previous configuration is
//! charged that configuration's figure.

use lmgw_core::runtime::Class;

use super::audio_residency::{
    add_audio_model, audio_resident, learn, models_page_note, note, op, readings_after, residency,
    speak, speak_and_read, start_idle, stretches_after, CONTEXT,
};
use super::image_pipeline_peak::{
    acquire_image, acquire_model, add_image_model, peak_of, ScriptedGpu,
};
use super::*;

/// Bring the container up as a warm start does, and wait for its reading at
/// rest — what every path that makes a container ready asks for
/// (`VramScheduler::cache_pids`).
async fn start_at_rest(f: &Fixture, model_id: &str) {
    let before = f.state.vram.residency_at_rest_readings();
    start_idle(f, model_id).await;
    f.state.vram.cache_pids(&f.state);
    for _ in 0..500 {
        if f.state.vram.residency_at_rest_readings() > before {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no reading at rest of {model_id}");
}

async fn in_flight_drains(f: &Fixture, model_id: &str) {
    for _ in 0..500 {
        let busy = f
            .state
            .runtime()
            .list()
            .iter()
            .any(|e| e.model_id == model_id && e.in_flight > 0);
        if !busy {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{model_id} still has a request in flight");
}

async fn set_audio(f: &Fixture, change: impl FnOnce(&mut lmgw_core::config::AudioSettings)) {
    let mut s = f.state.snapshot().settings.clone();
    change(&mut s.audio);
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// M1: a streaming-mode model's answer starts at its headers, while audio.cpp
/// may still be loading the weights — so it counts only once its relay ends
/// whole. A client that hangs up after the headers leaves the load pending
/// and teaches nothing; one that reads to the end teaches the figure.
#[tokio::test]
async fn a_streamed_answer_counts_only_once_its_relay_ends_whole() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    f.world().sse_bytes.insert("tts".into(), 32 * 1024 * 1024);
    let before = f.state.vram.residency_readings();

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/audio/speech", f.gateway))
        .json(&json!({"model": "audio/tts", "input": "hello", "voice": "alba"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    drop(resp);
    in_flight_drains(&f, "tts").await;

    let a = audio_resident(&vram_status(&f.gateway).await);
    assert_eq!(a["pending_bytes"], 2 * GIB, "not counted as loaded: {a}");
    assert_eq!(f.state.vram.residency_readings(), before, "nothing read");
    assert_eq!(residency(&f, "tts"), None);

    // Read to the end: now it has answered.
    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/audio/speech", f.gateway))
        .json(&json!({"model": "audio/tts", "input": "hello", "voice": "alba"}))
        .send()
        .await
        .unwrap();
    resp.bytes().await.unwrap();
    readings_after(&f, before).await;
    assert_eq!(residency(&f, "tts").map(|r| r.bytes), Some(3 * GIB));
    let a = audio_resident(&vram_status(&f.gateway).await);
    assert!(a["pending_bytes"].is_null(), "{a}");
}

/// The same for chunked PCM (audio-class gap 8): `stream_format: audio` is
/// answered as `application/octet-stream`, no event stream, and is streamed
/// all the same — it counts once its relay ends whole, and its request row
/// says streamed.
#[tokio::test]
async fn a_chunked_audio_answer_counts_only_once_its_relay_ends_whole() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    sqlx::query("UPDATE audio_models SET mode = 'streaming' WHERE id = ?1")
        .bind(id)
        .execute(&f.state.db)
        .await
        .unwrap();
    f.state.reload_snapshot().await.unwrap();
    f.world().sse_bytes.insert("tts".into(), 32 * 1024 * 1024);
    let before = f.state.vram.residency_readings();
    let ask = || {
        f.gateway
            .client()
            .post(format!("{}/v1/audio/speech", f.gateway))
            .json(
                &json!({"model": "audio/tts", "input": "hello", "voice": "alba",
                          "stream_format": "audio", "response_format": "pcm"}),
            )
            .send()
    };

    let resp = ask().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "application/octet-stream"
    );
    drop(resp);
    in_flight_drains(&f, "tts").await;
    assert_eq!(f.state.vram.residency_readings(), before, "nothing read");
    assert_eq!(residency(&f, "tts"), None);

    ask().await.unwrap().bytes().await.unwrap();
    readings_after(&f, before).await;
    assert_eq!(residency(&f, "tts").map(|r| r.bytes), Some(3 * GIB));
    let streamed: Vec<i64> =
        sqlx::query_scalar("SELECT streamed FROM request_logs WHERE class = 'audio' ORDER BY id")
            .fetch_all(&f.state.db)
            .await
            .unwrap();
    assert_eq!(streamed.last(), Some(&1), "{streamed:?}");
}

/// M2: the window opens against the previous idle tick's baseline. An audio
/// model that loaded between that tick and this one is in this tick's
/// `used` and not in the baseline, so its weights would be learned as the
/// pipeline's peak — unless the baseline's shape has to match.
#[tokio::test]
async fn an_audio_load_between_the_baseline_and_the_window_is_not_a_peak() {
    use lmgw_core::vram::peak::PeakSampler;

    const IDLE: u64 = 7 * GIB;
    let f = fixture(24 * GIB, 6 * GIB, 3 * GIB, 0).await;
    add_image_model(&f, "z-image", IDLE).await;
    add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    let used = Arc::new(Mutex::new(IDLE));
    f.state.vram.set_probe(Arc::new(ScriptedGpu {
        total: 24 * GIB,
        used: used.clone(),
    }));
    let set = |bytes: u64| *used.lock().unwrap() = bytes;
    let sampler = PeakSampler::new();
    start_idle(&f, "tts").await;
    drop(acquire_image(&f, "z-image").await);
    sampler.tick(&f.state).await; // the baseline, TTS not loaded

    // Between two ticks: the TTS loads, then a generation starts.
    assert_eq!(speak(&f, "tts").await, 200);
    set(IDLE + 3 * GIB);
    let guard = acquire_image(&f, "z-image").await;
    sampler.tick(&f.state).await;
    set(IDLE + 3 * GIB + GIB);
    sampler.tick(&f.state).await;
    drop(guard);
    set(IDLE + 3 * GIB);
    sampler.tick(&f.state).await;
    assert_eq!(
        peak_of(&f, "z-image").await,
        None,
        "the TTS weights are not the pipeline's peak"
    );

    // That last tick took a baseline that saw the TTS loaded: the next
    // window learns the generation alone.
    let guard = acquire_image(&f, "z-image").await;
    sampler.tick(&f.state).await;
    set(IDLE + 3 * GIB + 2 * GIB);
    sampler.tick(&f.state).await;
    drop(guard);
    set(IDLE + 3 * GIB);
    sampler.tick(&f.state).await;
    assert_eq!(peak_of(&f, "z-image").await, Some(2 * GIB));
}

/// M3: after `idle_unload_ms` audio.cpp has unloaded the model, and the next
/// request reloads it. A request in flight does not mean "loaded": until it
/// answers, the reload is pending like the first load.
#[tokio::test]
async fn an_unloaded_model_s_reload_is_pending_while_it_runs() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    set_audio(&f, |a| a.idle_unload_ms = 200).await;

    assert_eq!(speak(&f, "tts").await, 200);
    let a = audio_resident(&vram_status(&f.gateway).await);
    assert!(a["pending_bytes"].is_null(), "just answered: {a}");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let a = audio_resident(&vram_status(&f.gateway).await);
    assert_eq!(a["pending_bytes"], 2 * GIB, "{a}");
    assert!(note(&a).contains("Unloaded after idling 200 ms"), "{a}");

    let claim = acquire_model(&f, Class::Audio, "tts").await;
    let a = audio_resident(&vram_status(&f.gateway).await);
    assert_eq!(a["in_flight"], 1);
    assert_eq!(
        a["pending_bytes"],
        2 * GIB,
        "a request in flight on an unloaded model is a reload, still pending: {a}"
    );
    drop(claim);
}

/// M4: a ready container that has not loaded holds its CUDA context, and
/// the driver shows it. What is pending is the rest: the expected residency
/// less what it holds at rest.
#[tokio::test]
async fn a_warm_container_s_context_is_not_counted_twice() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    learn(&f, id, "tts", 3 * GIB).await;
    start_at_rest(&f, "tts").await;

    let v = vram_status(&f.gateway).await;
    let a = audio_resident(&v);
    assert_eq!(a["pending_bytes"], 3 * GIB - CONTEXT, "{a}");
    assert_eq!(
        v["free_bytes"],
        10 * GIB - 3 * GIB,
        "the context once, in the driver's figure: {v}"
    );
    assert!(
        note(&a).contains("it holds 256.0 MiB at rest already"),
        "{a}"
    );
}

/// M4: a container a previous lmgw left running, which had answered
/// requests then, holds far more than a bare context when it is adopted: it
/// is taken as loaded, and nothing is pending. (Before, all three voice
/// models were charged their whole residency twice after every restart.)
#[tokio::test]
async fn an_adopted_container_holding_its_model_is_taken_as_loaded() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    learn(&f, id, "tts", 3 * GIB).await;
    f.world().size.insert("tts".into(), 3 * GIB);
    start_at_rest(&f, "tts").await;

    let v = vram_status(&f.gateway).await;
    let a = audio_resident(&v);
    assert!(a["pending_bytes"].is_null(), "{a}");
    assert_eq!(v["free_bytes"], 7 * GIB);
    assert!(
        note(&a).contains("Taken as loaded: its processes held 3.0 GiB at rest"),
        "{a}"
    );
}

/// M4, eager: its weights are on the card at rest, and the reading says how
/// much — the rest of its learned figure is what is pending.
#[tokio::test]
async fn an_eager_container_read_at_rest_keeps_free_the_rest_of_its_figure() {
    let f = fixture(10 * GIB, 6 * GIB, GIB, 0).await;
    f.attribute(0);
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, Some(false)).await;
    learn(&f, id, "tts", 3 * GIB).await;
    f.world().size.insert("tts".into(), 2 * GIB + GIB / 4);
    start_at_rest(&f, "tts").await;

    let a = audio_resident(&vram_status(&f.gateway).await);
    assert_eq!(a["pending_bytes"], 3 * GIB - 2 * GIB - GIB / 4, "{a}");
    assert!(
        note(&a).contains("Weights loaded, not run yet") && note(&a).contains("at rest"),
        "{a}"
    );
}

/// Low: a reading that found no figure says why — on the resident and on
/// the models page — and the next answered request reads again.
#[tokio::test]
async fn a_failed_reading_says_why_and_the_next_answer_reads_again() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    f.world().unlisted.insert("tts".into());

    speak_and_read(&f, "tts").await;
    assert_eq!(residency(&f, "tts"), None);
    let a = audio_resident(&vram_status(&f.gateway).await);
    assert!(
        note(&a).contains("found no figure (the driver lists none of its processes"),
        "{a}"
    );
    assert!(models_page_note(&f, "tts")
        .await
        .contains("the next answered request reads again"));

    f.world().unlisted.remove("tts");
    speak_and_read(&f, "tts").await;
    assert_eq!(residency(&f, "tts").map(|r| r.bytes), Some(3 * GIB));
    let a = audio_resident(&vram_status(&f.gateway).await);
    assert!(!note(&a).contains("found no figure"), "{a}");
}

/// A reading that keeps failing for the same reason is said at INFO once
/// per container start, and at DEBUG after that. "Is this failure new?" is
/// asked before the reading's end records it: asked after, it would never be
/// new, and the line would never be said at all.
#[tokio::test]
async fn a_failed_reading_is_said_at_info_once_for_its_container_and_reason() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    f.world().unlisted.insert("tts".into());

    let (log, capturing) = crate::common::captured_log::capture_log();
    speak_and_read(&f, "tts").await;
    speak_and_read(&f, "tts").await;
    drop(capturing);
    let log = log.text();
    assert_eq!(
        log.matches("audio/tts: residency not read after its request")
            .count(),
        1,
        "{log}"
    );
}

/// Low: the owner's reset while a reading is in flight. The reading began
/// before the reset, so its figure is not written back after it. (The reset
/// is an update, which stops an idle container — and a stopped container's
/// reading is dropped anyway; a busy one keeps running, which is the race.)
#[tokio::test]
async fn a_reset_drops_a_reading_that_began_before_it() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    let id = add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    learn(&f, id, "tts", GIB).await;
    let (open, gate) = tokio::sync::watch::channel(false);
    *f.podman.inspect_gate.lock().unwrap() = Some(gate);
    let (readings, stretches) = (
        f.state.vram.residency_readings(),
        f.state.vram.residency_stretches(),
    );

    // The request is answered; its reading waits on the container's PID.
    assert_eq!(speak(&f, "tts").await, 200);
    in_flight_drains(&f, "tts").await;
    assert_eq!(f.state.vram.residency_readings(), readings);
    // Another request keeps the container busy through the reset.
    let busy = acquire_model(&f, Class::Audio, "tts").await;
    let out = op(
        &f,
        "audio_model_set",
        json!({"action": "update", "id": id, "clear": "residency"}),
    )
    .await;
    assert_eq!(out["residency_reset"], true, "{out}");
    assert!(
        f.state.runtime().contains(Class::Audio, "tts"),
        "still running"
    );

    open.send(true).unwrap();
    readings_after(&f, readings).await;
    drop(busy);
    stretches_after(&f, stretches).await;
    assert_eq!(
        residency(&f, "tts"),
        None,
        "a figure read before the reset is not written back"
    );
}

/// Low: with the outside-VRAM trigger off, the readings' PID plans are the
/// only thing that retires a stopped container into tombstones — so the
/// readings settle them too, or they pile up.
#[tokio::test]
async fn tombstones_are_settled_with_the_trigger_off() {
    let f = fixture(10 * GIB, 2 * GIB, GIB, 0).await;
    f.attribute(0);
    let mut s = f.state.snapshot().settings.clone();
    s.vram.fallback_on_external = false;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    add_audio_model(&f, "tts", GIB, 2 * GIB, None).await;
    add_audio_model(&f, "asr", GIB, 2 * GIB, None).await;

    speak_and_read(&f, "tts").await;
    f.state
        .runtime()
        .stop(Class::Audio, "tts", false)
        .await
        .unwrap();
    speak_and_read(&f, "asr").await;
    assert_eq!(
        f.state.vram.tombstones(),
        0,
        "the stopped container's processes are gone from the driver's list"
    );
}

/// Low: an eager row nothing could be read of keeps free what comes on top
/// of the weights file it loads — not of its directory, which may hold
/// several variants of which audio.cpp loads one.
#[tokio::test]
async fn an_eager_row_subtracts_its_weights_file_not_its_directory() {
    let f = fixture(10 * GIB, 6 * GIB, GIB, 0).await;
    let id = add_audio_model(&f, "tts", GIB, 3 * GIB, Some(false)).await;
    std::fs::File::create(f._models_dir.path().join("tts").join("model-f16.gguf"))
        .unwrap()
        .set_len(2 * GIB)
        .unwrap();
    learn(&f, id, "tts", 3 * GIB).await;
    start_idle(&f, "tts").await;

    let a = audio_resident(&vram_status(&f.gateway).await);
    assert_eq!(
        a["pending_bytes"],
        2 * GIB,
        "3 GiB less the 1 GiB file: {a}"
    );
    assert!(note(&a).contains("above its 1.0 GiB weights file"), "{a}");
}

/// Low: a class-level change re-keys the row while the running container
/// keeps the configuration it was started with — and is charged that
/// configuration's stored figure, not the files.
#[tokio::test]
async fn a_container_running_a_previous_configuration_is_charged_its_figure() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    speak_and_read(&f, "tts").await;
    set_audio(&f, |a| a.image = "example.org/audio.cpp:other".into()).await;

    let a = audio_resident(&vram_status(&f.gateway).await);
    assert_eq!(a["estimated_bytes"], 3 * GIB, "{a}");
    assert!(
        note(&a).contains("still runs the configuration that figure was learned for"),
        "{a}"
    );
}
