//! Auto headroom from a recorded history, the fixed value before it, and
//! the margin of a model that is starting or not measured yet.

use super::*;
use crate::config::AudioModel;
use crate::vram::nvml::NoTelemetry;

const GIB: u64 = 1024 * MIB;

fn snap(mode: HeadroomMode) -> Snapshot {
    let mut s = Snapshot::default();
    s.settings.vram.headroom_mode = mode;
    s.settings.vram.headroom_mb = 1024;
    s
}

/// `hours` hours of outside use, 130 MiB at rest and a rise to 770 MiB in
/// the last one.
fn history(v: &VramScheduler, hours: u64) {
    let now = growth::now_hour();
    let mut g = v.growth.lock().unwrap();
    for h in 0..hours {
        let at = now - (hours - 1 - h) as i64;
        g.record(at, 130 * MIB);
    }
    g.record(now, 770 * MIB);
}

#[test]
fn auto_is_the_largest_growth_once_a_day_is_recorded() {
    let v = VramScheduler::new();
    history(&v, HOURS_NEEDED);
    let s = snap(HeadroomMode::Auto);
    assert_eq!(
        v.headroom(&s),
        Headroom {
            bytes: 640 * MIB,
            learned: true
        }
    );
    let now = v.headroom_now(&s);
    assert!(now.learned_in_use);
    assert_eq!(now.learned_bytes, Some(640 * MIB));
    assert!(
        now.sentence
            .starts_with("Headroom: auto, 640 MiB: other programs grew by up to that much, on "),
        "{}",
        now.sentence
    );
    // Fixed mode ignores it.
    let fixed = snap(HeadroomMode::Fixed);
    assert_eq!(
        v.headroom(&fixed),
        Headroom {
            bytes: GIB,
            learned: false
        }
    );
    assert!(v
        .headroom_now(&fixed)
        .sentence
        .starts_with("Headroom: fixed, 1.0 GiB"));
}

/// A fresh install: the fixed value, no margin, and Settings says how long
/// is left and what was seen so far.
#[test]
fn before_a_day_is_recorded_auto_uses_the_fixed_value() {
    let v = VramScheduler::new();
    history(&v, 3);
    let s = snap(HeadroomMode::Auto);
    assert_eq!(
        v.headroom(&s),
        Headroom {
            bytes: GIB,
            learned: false
        }
    );
    let fp = Footprint::new(2 * GIB, 0, None, None);
    assert_eq!(
        v.margin(&s, Class::Chat, "m", &fp),
        0,
        "the fixed value covers it"
    );
    assert_eq!(v.start_headroom(&s, Class::Chat, "m", &fp), GIB);
    let now = v.headroom_now(&s);
    assert!(!now.learned_in_use);
    // A probe without per-process figures says so instead.
    v.set_probe(std::sync::Arc::new(NoTelemetry("no driver".into())));
    let now_without = v.headroom_now(&s);
    assert!(
        now_without
            .sentence
            .contains("gives no per-program GPU memory figures"),
        "{}",
        now_without.sentence
    );
    let _ = now;
}

/// The learned headroom in use: a starting model gets its class's margin;
/// one that is measured — an audio row with its residency learned, an image
/// row whose context is measured — gets none.
#[test]
fn the_margin_is_the_class_gap_and_drops_once_measured() {
    let v = VramScheduler::new();
    history(&v, HOURS_NEEDED);
    let mut s = snap(HeadroomMode::Auto);
    let fp = Footprint::new(2 * GIB, 0, None, None);
    assert_eq!(v.margin(&s, Class::Chat, "m", &fp), LLAMA_MARGIN);
    assert_eq!(
        v.start_headroom(&s, Class::Chat, "m", &fp),
        640 * MIB + LLAMA_MARGIN
    );
    assert_eq!(v.margin(&s, Class::Image, "m", &fp), CONTEXT_STAND_IN);

    // Audio: as much as its estimate until its residency is learned.
    let row = AudioModel {
        id: 1,
        model_id: "asr".into(),
        family: "parakeet_tdt".into(),
        path: "asr".into(),
        task: "asr".into(),
        mode: "offline".into(),
        lazy: None,
        busy_timeout_ms: None,
        backend: None,
        threads: None,
        default_request_options: Default::default(),
        model_spec_override: None,
        config_id: None,
        weight_id: None,
        load_options: Default::default(),
        session_options: Default::default(),
        voice_presets: Default::default(),
        default_voice_preset: None,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        fallback: None,
        residency: None,
    };
    s.audio_models = vec![row.clone()];
    assert_eq!(v.margin(&s, Class::Audio, "asr", &fp), 2 * GIB);
    let small = Footprint::new(100 * MIB, 0, None, None);
    assert_eq!(v.margin(&s, Class::Audio, "asr", &small), AUDIO_MARGIN);
    let key = crate::vram::residency::resident_key(&row, &s.settings.audio);
    let mut learned = row;
    learned.residency = Some(crate::config::LearnedResidency {
        key,
        bytes: 3 * GIB,
        learned_at: "2026-11-11 00:00:00".into(),
    });
    s.audio_models = vec![learned];
    assert_eq!(v.margin(&s, Class::Audio, "asr", &fp), 0, "measured");
}

/// A game run without the hold taught 18.5 GiB of growth (review finding
/// 3): admission keeps that free now, but whether a model can run at all is
/// judged against what other programs hold at rest and the margin. The
/// sentence names when it was learned and every figure behind it, and the
/// growth can be forgotten.
#[test]
fn a_large_growth_decides_room_now_and_never_too_large() {
    let v = VramScheduler::new();
    let now = growth::now_hour();
    {
        let mut g = v.growth.lock().unwrap();
        for h in 0..HOURS_NEEDED {
            g.record(now - (HOURS_NEEDED - 1 - h) as i64, 130 * MIB);
        }
        g.record(now, 130 * MIB + 18 * GIB + 512 * MIB);
    }
    let s = snap(HeadroomMode::Auto);
    let fp = Footprint::new(2 * GIB, 0, None, None);
    assert_eq!(
        v.start_headroom(&s, Class::Chat, "m", &fp),
        18 * GIB + 512 * MIB + LLAMA_MARGIN
    );
    assert_eq!(
        v.card_headroom(&s, Class::Chat, "m", &fp),
        130 * MIB + LLAMA_MARGIN,
        "at rest and the margin, never the growth"
    );
    assert_eq!(
        v.start_margin(&s, Class::Chat, "m", &fp),
        LLAMA_MARGIN,
        "what a start needs beside it once nothing of lmgw's is left"
    );
    // Once nothing of lmgw's is left, a start without the growth beside it
    // is warned with the growth and when it was seen; one that has it is not.
    let t = crate::vram::Target {
        class: Class::Chat,
        model_id: "m".into(),
    };
    let kept = (v.start_headroom(&s, Class::Chat, "m", &fp), LLAMA_MARGIN);
    let note = v
        .growth_let_go(&t, 2 * GIB, kept, 4 * GIB)
        .expect("the growth was let go");
    assert!(
        note.starts_with(GROWTH_LET_GO) && note.contains("18.5 GiB on ") && note.contains("chat/m"),
        "{note}"
    );
    assert_eq!(v.growth_let_go(&t, 2 * GIB, kept, 30 * GIB), None);
    let said = v.headroom_now(&s);
    assert_eq!(said.at_rest_bytes, Some(130 * MIB));
    assert_eq!(said.sample_seconds, 30);
    let at = said.learned_at.clone().expect("when it was learned");
    for part in [
        "Headroom: auto, 18.5 GiB: other programs grew by up to that much, on ",
        at.as_str(),
        "a reading every 30 s",
        "the last 7 days",
        "once 24 hours have readings",
        "807 MiB for a llama.cpp model",
        "1.0 GiB for an image model",
        "at least 799 MiB for a voice model",
        "what other programs hold at rest (130 MiB)",
        "The growth only makes a start wait for or evict lmgw's own models",
    ] {
        assert!(
            said.sentence.contains(part),
            "{part:?} in {}",
            said.sentence
        );
    }
    v.growth.lock().unwrap().forget();
    assert_eq!(v.headroom(&s).bytes, GIB, "the fixed value again");
}
