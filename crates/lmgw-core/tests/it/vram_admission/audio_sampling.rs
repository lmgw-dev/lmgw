//! Sampling an audio container while a request runs on it (fix package B7,
//! the WP7 live gate): audio.cpp frees part of what a request takes once it
//! has answered — pocket-tts read 0.921 GB after its answer and 0.981 GB at
//! most while it worked — so the figure is the larger of what was sampled
//! and what was read after the answer, and sampling stops once the figure
//! has settled.

use lmgw_core::vram::residency::SETTLE_AFTER;

use super::audio_residency::{
    add_audio_model, audio_resident, models_page_note, note, readings_after, residency, speak,
};
use super::*;

const MIB: u64 = 1024 * 1024;

async fn stretches_after(f: &Fixture, before: u64) {
    for _ in 0..500 {
        if f.state.vram.residency_stretches() > before {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no sampler stretch ended after {before}");
}

/// One request, and wait for both its reading after the answer and the end
/// of the sampler's stretch (when one ran).
async fn speak_sampled(f: &Fixture, model_id: &str, sampled: bool) {
    let (readings, stretches) = (
        f.state.vram.residency_readings(),
        f.state.vram.residency_stretches(),
    );
    assert_eq!(speak(f, model_id).await, 200);
    readings_after(f, readings).await;
    if sampled {
        stretches_after(f, stretches).await;
    }
}

/// The transient the reading after the answer misses is learned from the
/// samples taken while the request ran.
#[tokio::test]
async fn what_a_request_takes_while_it_runs_is_learned() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    f.world()
        .transient
        .insert("tts".into(), Transient::sampled(3 * GIB + 60 * MIB));

    speak_sampled(&f, "tts", true).await;
    assert_eq!(
        residency(&f, "tts").map(|r| r.bytes),
        Some(3 * GIB + 60 * MIB),
        "the in-flight maximum, not the 3 GiB left after the answer"
    );
    assert_eq!(
        f.world().size["tts"],
        3 * GIB,
        "the fake did free its buffers"
    );
}

/// Bounded: once `SETTLE_AFTER` sampled requests in a row leave the figure
/// where it was, that configuration's requests are not sampled any more —
/// the reading after each answer goes on, and the surfaces say so.
#[tokio::test]
async fn sampling_stops_once_the_figure_has_settled() {
    let f = fixture(10 * GIB, 8 * GIB, GIB, 0).await;
    f.attribute(0);
    add_audio_model(&f, "tts", 2 * GIB, 3 * GIB, None).await;
    // Held until the sampler has read it: every sampled request learns it,
    // however loaded the box is.
    f.world()
        .transient
        .insert("tts".into(), Transient::sampled(3 * GIB + 60 * MIB));

    speak_sampled(&f, "tts", true).await;
    let learned = 3 * GIB + 60 * MIB;
    assert_eq!(residency(&f, "tts").map(|r| r.bytes), Some(learned));
    assert!(
        models_page_note(&f, "tts").await.contains("(0 so far)"),
        "a rise starts the count"
    );
    for _ in 0..SETTLE_AFTER {
        speak_sampled(&f, "tts", true).await;
    }
    let n = models_page_note(&f, "tts").await;
    assert!(n.contains("settled"), "{n}");
    assert!(
        note(&audio_resident(&vram_status(&f.gateway).await)).contains("settled"),
        "the resident says it too"
    );

    // A larger transient now is not sampled: only the reading after the
    // answer, which sees the buffers already freed (dropped before the
    // answer leaves, whatever the wall clock does).
    f.world().transient.insert(
        "tts".into(),
        Transient::lasting(3 * GIB + 500 * MIB, Duration::from_millis(150)),
    );
    let stretches = f.state.vram.residency_stretches();
    speak_sampled(&f, "tts", false).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        f.state.vram.residency_stretches(),
        stretches,
        "no sampler ran"
    );
    assert_eq!(residency(&f, "tts").map(|r| r.bytes), Some(learned));
}
