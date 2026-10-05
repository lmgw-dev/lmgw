//! The residency rules that need no gateway: the key, the charge, what is
//! pending, and what the surfaces say.

use serde_json::json;

use std::time::{Duration, Instant};

use super::tracker::{AtRest, GenView, Noted, Stretch};
use super::*;
use crate::config::{AudioModel, AudioSettings, LearnedResidency};

const GIB: u64 = 1024 * 1024 * 1024;

fn row() -> AudioModel {
    AudioModel {
        id: 1,
        model_id: "tts".into(),
        family: "pocket_tts".into(),
        path: "pocket".into(),
        task: "tts".into(),
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
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        residency: None,
    }
}

fn map(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().unwrap().clone()
}

/// A row that learned `bytes` under its current configuration.
fn learned_row(bytes: u64) -> AudioModel {
    let mut m = row();
    m.residency = Some(LearnedResidency {
        bytes,
        learned_at: "2026-10-01 12:00:00".into(),
        key: resident_key(&m, &AudioSettings::default()),
    });
    m
}

fn files(on_disk: u64) -> Footprint {
    Footprint {
        weights_bytes: on_disk,
        kv_cache_bytes: 0,
        total_bytes: on_disk,
        ctx_tokens: None,
        note: Some("the on-disk size".into()),
    }
}

/// One edit of a row or of the class settings, named.
type Change = (
    &'static str,
    Box<dyn Fn(&mut AudioModel, &mut AudioSettings)>,
);

/// Everything that changes what audio.cpp loads changes the key; nothing
/// else does. A key that moved on a voice preset would throw away a learned
/// figure on every voice edit; one that stayed put on a session option (the
/// quantization) would charge a q4 model's figure for its f16 weights.
#[test]
fn the_key_follows_what_is_loaded_and_nothing_else() {
    let s = AudioSettings::default();
    let base = resident_key(&row(), &s);
    assert_eq!(base.len(), 12);
    assert!(base.chars().all(|c| c.is_ascii_hexdigit()), "{base}");

    let changes: Vec<Change> = vec![
        ("family", Box::new(|m, _| m.family = "qwen3_tts".into())),
        ("path", Box::new(|m, _| m.path = "pocket-q4".into())),
        ("task", Box::new(|m, _| m.task = "asr".into())),
        ("mode", Box::new(|m, _| m.mode = "streaming".into())),
        ("config id", Box::new(|m, _| m.config_id = Some("c".into()))),
        ("weight id", Box::new(|m, _| m.weight_id = Some("w".into()))),
        (
            "spec override",
            Box::new(|m, _| m.model_spec_override = Some("specs".into())),
        ),
        (
            "load options",
            Box::new(|m, _| m.load_options = map(json!({"language": "de"}))),
        ),
        (
            "session options",
            Box::new(|m, _| m.session_options = map(json!({"weight_type": "q4_0"}))),
        ),
        (
            "row image",
            Box::new(|m, _| m.image = Some("my/audio".into())),
        ),
        (
            "class image",
            Box::new(|_, s| s.image = "other/audio".into()),
        ),
        (
            "row run args",
            Box::new(|m, _| m.extra_run_args = Some(vec!["--device".into(), "cpu".into()])),
        ),
        (
            "class run args",
            Box::new(|_, s| s.extra_run_args = vec!["--cpus".into(), "2".into()]),
        ),
        ("backend", Box::new(|_, s| s.backend = "cpu".into())),
        ("device", Box::new(|_, s| s.device = 1)),
    ];
    for (what, change) in changes {
        let (mut m, mut s) = (row(), AudioSettings::default());
        change(&mut m, &mut s);
        assert_ne!(resident_key(&m, &s), base, "{what} changes what is loaded");
    }

    let same: Vec<Change> = vec![
        (
            "voice presets",
            Box::new(|m, _| m.voice_presets = map(json!({"alba": {"voice_id": "alba"}}))),
        ),
        (
            "default voice",
            Box::new(|m, _| m.default_voice_preset = Some(json!("alba"))),
        ),
        (
            "request defaults",
            Box::new(|m, _| m.default_request_options = map(json!({"speed": 1.2}))),
        ),
        ("busy timeout", Box::new(|m, _| m.busy_timeout_ms = Some(5))),
        ("row lazy", Box::new(|m, _| m.lazy = Some(false))),
        ("class lazy", Box::new(|_, s| s.lazy_load = false)),
        ("idle unload", Box::new(|_, s| s.idle_unload_ms = 60_000)),
        ("threads", Box::new(|_, s| s.threads = 8)),
        ("min free", Box::new(|_, s| s.min_free_memory_mb = 512)),
        ("warm start", Box::new(|m, _| m.warm_start = true)),
        ("enabled", Box::new(|m, _| m.enabled = false)),
        (
            "hold fallback",
            Box::new(|m, _| m.hold_fallback = Some("x".into())),
        ),
        (
            "a row image equal to the class's",
            Box::new(|m, s| m.image = Some(s.image.clone())),
        ),
    ];
    for (what, change) in same {
        let (mut m, mut s) = (row(), AudioSettings::default());
        change(&mut m, &mut s);
        assert_eq!(resident_key(&m, &s), base, "{what} loads nothing");
    }
}

/// The owner's option maps are compared by content, not by the order they
/// were written in.
#[test]
fn the_key_does_not_depend_on_option_order() {
    let s = AudioSettings::default();
    let (mut a, mut b) = (row(), row());
    let mut first = serde_json::Map::new();
    first.insert("language".into(), json!("de"));
    first.insert("voice".into(), json!({"z": 1, "a": 2}));
    let mut second = serde_json::Map::new();
    second.insert("voice".into(), json!({"a": 2, "z": 1}));
    second.insert("language".into(), json!("de"));
    a.load_options = first;
    b.load_options = second;
    assert_eq!(resident_key(&a, &s), resident_key(&b, &s));
}

/// The charge: the learned figure while it belongs to the row's
/// configuration, the on-disk size otherwise — never a multiple of it.
#[test]
fn a_row_is_charged_its_learned_figure_only_for_its_own_configuration() {
    let s = AudioSettings::default();
    let m = learned_row(GIB);
    let fp = expected(files(GIB / 8), &m, &s);
    assert_eq!(fp.total_bytes, GIB);
    assert_eq!(fp.weights_bytes, GIB / 8, "the files stay the files");
    assert!(fp.note.unwrap().contains("learned residency"));

    let mut changed = m.clone();
    changed.session_options = map(json!({"weight_type": "q4_0"}));
    assert_eq!(learned(&changed, &s), None);
    let fp = expected(files(GIB / 8), &changed, &s);
    assert_eq!(fp.total_bytes, GIB / 8, "a previous configuration's figure");

    assert_eq!(expected(files(GIB / 8), &row(), &s).total_bytes, GIB / 8);
}

fn at_rest(r: AtRest) -> GenView {
    GenView {
        at_rest: r,
        ..GenView::default()
    }
}

fn answered(ago: Duration, r: AtRest) -> (GenView, Instant) {
    let now = Instant::now() + Duration::from_secs(3600);
    (
        GenView {
            last_answered: Some(now - ago),
            at_rest: r,
            failure: None,
        },
        now,
    )
}

/// What a ready container that has not loaded still takes: its expected
/// residency less what it holds at rest when that was read (WP7 review M4),
/// all of it for a lazy row nothing was read of, what comes on top of its
/// selected weights file for an eager one — and nothing once it answered.
#[test]
fn the_pending_table() {
    let lazy_class = AudioSettings::default();
    let eager_class = AudioSettings {
        lazy_load: false,
        ..AudioSettings::default()
    };
    let now = Instant::now();
    let learned = learned_row(3 * GIB);
    let p =
        |m: &AudioModel, s: &AudioSettings, g: &GenView| pending(m, s, 3 * GIB, GIB, g, now).bytes;

    assert_eq!(p(&learned, &lazy_class, &GenView::default()), 3 * GIB);
    assert_eq!(
        pending(
            &learned,
            &lazy_class,
            3 * GIB,
            GIB,
            &GenView::default(),
            now
        )
        .basis,
        Basis::Whole
    );
    assert_eq!(
        p(&learned, &lazy_class, &at_rest(AtRest::Bare(GIB / 4))),
        3 * GIB - GIB / 4,
        "the context it holds at rest is in the driver's figure already"
    );
    assert_eq!(
        p(&learned, &lazy_class, &at_rest(AtRest::Failed("x".into()))),
        3 * GIB,
        "a failed reading charges all of it"
    );
    let (loaded, at) = answered(Duration::from_secs(1), AtRest::Unread);
    assert_eq!(
        pending(&learned, &lazy_class, 3 * GIB, 0, &loaded, at).bytes,
        0
    );

    let mut eager = learned.clone();
    eager.lazy = Some(false);
    assert_eq!(
        p(&eager, &lazy_class, &GenView::default()),
        2 * GIB,
        "nothing read: what comes on top of the weights file"
    );
    assert_eq!(
        pending(&eager, &lazy_class, 3 * GIB, GIB, &GenView::default(), now).basis,
        Basis::AboveWeights { weights: GIB }
    );
    assert_eq!(
        p(&eager, &lazy_class, &at_rest(AtRest::Eager(GIB + GIB / 2))),
        GIB + GIB / 2,
        "read at rest: what it holds, weights and context, is subtracted"
    );
    assert_eq!(
        pending(&row(), &eager_class, GIB, GIB, &GenView::default(), now).bytes,
        0,
        "an eager row's weights are on the card once it is ready"
    );
    let mut lazy_row = row();
    lazy_row.lazy = Some(true);
    assert_eq!(
        pending(&lazy_row, &eager_class, GIB, GIB, &GenView::default(), now).bytes,
        GIB
    );
    assert_eq!(
        p(&learned, &lazy_class, &at_rest(AtRest::Bare(4 * GIB))),
        0,
        "never below nothing"
    );
}

/// audio.cpp's `idle_unload_ms` gives the card back while the container
/// stays up. Holding is "answered, and not idle for that long since" — a
/// request in flight does not count (WP7 review M3): an unloaded model's
/// next request is a reload, pending like the first load until it answers.
#[test]
fn an_idle_unload_makes_a_loaded_container_pending_again() {
    let now = Instant::now() + Duration::from_secs(3600);
    let never = AudioSettings::default();
    assert!(holds(Some(now - Duration::from_secs(10_000)), now, &never));
    assert!(!holds(None, now, &never));
    let minute = AudioSettings {
        idle_unload_ms: 60_000,
        ..AudioSettings::default()
    };
    assert!(holds(Some(now - Duration::from_secs(59)), now, &minute));
    assert!(!holds(Some(now - Duration::from_secs(60)), now, &minute));

    // Unloaded: the expected figure less the bare context read at rest.
    let learned = learned_row(3 * GIB);
    let (g, at) = answered(Duration::from_secs(61), AtRest::Bare(GIB / 4));
    let p = pending(&learned, &minute, 3 * GIB, 0, &g, at);
    assert_eq!(p.bytes, 3 * GIB - GIB / 4);
    assert_eq!(
        p.basis,
        Basis::Unloaded {
            bare: Some(GIB / 4)
        }
    );
    let (g, at) = answered(Duration::from_secs(61), AtRest::Unread);
    assert_eq!(
        pending(&learned, &minute, 3 * GIB, 0, &g, at).bytes,
        3 * GIB
    );
}

/// The at-rest rule: more than a bare context plus half the files is a
/// loaded model; the bare contexts and the smallest loaded model measured
/// (§3.1) fall on the right sides of it.
#[test]
fn the_at_rest_rule_tells_a_bare_context_from_a_loaded_model() {
    const MIB: u64 = 1024 * 1024;
    assert!(!looks_loaded(300 * MIB, 122 * MIB), "a bare context");
    assert!(!looks_loaded(BARE_CONTEXT_CEILING, 0));
    assert!(
        looks_loaded(768 * MIB, 122 * MIB),
        "pocket-tts-english loaded"
    );
    assert!(looks_loaded(1478 * MIB, 888 * MIB), "nemotron loaded");
    assert!(
        !looks_loaded(700 * MIB, 2358 * MIB),
        "qwen3-asr's line sits well above any context"
    );
}

#[test]
fn the_notes_say_what_is_charged_and_why() {
    let s = AudioSettings::default();
    let calm = Sampling::default();
    let learned = learned_row(GIB);
    let n = residency_sentence(&learned, &s, GIB / 8, None, calm);
    assert!(
        n.contains("resident 1.0 GiB learned 2026-10-01 12:00:00"),
        "{n}"
    );
    assert!(
        n.contains("sampled while they run until 3 in a row do not raise it (0 so far)"),
        "{n}"
    );
    let n = residency_sentence(&learned, &s, GIB / 8, None, Sampling { calm: 3 });
    assert!(n.contains("settled"), "{n}");

    let mut previous = learned.clone();
    previous.path = "elsewhere".into();
    let n = residency_sentence(&previous, &s, GIB / 8, None, calm);
    assert!(n.contains("learned for a previous configuration"), "{n}");
    assert!(n.contains("on-disk 128.0 MiB"), "{n}");

    let n = residency_sentence(&row(), &s, GIB / 8, None, calm);
    assert!(
        n.contains("not learned")
            && n.contains("measured to exceed (CUDA context, compute buffers)")
            && n.contains("first request teaches it"),
        "{n}"
    );
    let n = residency_sentence(
        &row(),
        &s,
        GIB / 8,
        Some("FakeGPU lists no processes"),
        calm,
    );
    assert!(
        n.contains("cannot learn: FakeGPU lists no processes"),
        "{n}"
    );

    let now = Instant::now();
    let note = |m: &AudioModel, g: &GenView, cannot: Option<&str>| {
        let p = pending(m, &s, GIB, GIB / 2, g, now);
        resident_note(
            m,
            &s,
            &NoteFacts {
                on_disk: GIB / 8,
                pending: &p,
                cannot_learn: cannot,
                sampling: calm,
                gen: g,
                runs_previous: false,
            },
        )
    };
    let n = note(&row(), &GenView::default(), Some("no NVML"));
    assert!(
        n.contains("Not loaded yet: 1.0 GiB kept free until its first request")
            && n.contains("all of it, because this host gives no per-process figures (no NVML)"),
        "{n}"
    );
    let n = note(&row(), &at_rest(AtRest::Bare(GIB / 4)), None);
    assert!(
        n.contains("768.0 MiB kept free") && n.contains("it holds 256.0 MiB at rest already"),
        "{n}"
    );
    let n = note(
        &row(),
        &at_rest(AtRest::Failed("podman said no".into())),
        None,
    );
    assert!(n.contains("could not be read: podman said no"), "{n}");
    let mut eager = learned.clone();
    eager.lazy = Some(false);
    let n = note(&eager, &GenView::default(), None);
    assert!(
        n.contains("512.0 MiB more kept free") && n.contains("above its 512.0 MiB weights file"),
        "{n}"
    );
    let loaded = GenView {
        last_answered: Some(now),
        at_rest: AtRest::Loaded(GIB),
        failure: Some("the driver lists none of its processes".into()),
    };
    let n = note(&learned, &loaded, None);
    assert!(
        n.contains("Taken as loaded: its processes held 1.0 GiB"),
        "{n}"
    );
    assert!(
        n.contains("found no figure (the driver lists none of its processes); the next answered request reads again"),
        "{n}"
    );
}

/// One reading in flight per generation; the loaded mark outlives the
/// reading and dies with the generation; a failure is kept until a reading
/// finds a figure.
#[test]
fn the_tracker_marks_loads_and_one_reading_at_a_time() {
    let r = Residency::default();
    assert_eq!(
        r.note(3),
        Noted {
            newly_loaded: true,
            measure: true
        }
    );
    assert_eq!(
        r.note(3),
        Noted {
            newly_loaded: false,
            measure: false
        },
        "a reading is in flight"
    );
    assert!(r.failure_is_new(3, "no PID"), "nothing failed yet");
    r.measured(3, Err("no PID".into()));
    assert_eq!(r.get(3).unwrap().failure.as_deref(), Some("no PID"));
    // The same reason again is not news (logged once per container); another
    // one is, and so is the same reason on another container.
    assert!(!r.failure_is_new(3, "no PID"));
    assert!(r.failure_is_new(3, "no figure"));
    assert!(r.failure_is_new(4, "no PID"));
    assert_eq!(
        r.note(3),
        Noted {
            newly_loaded: false,
            measure: true
        },
        "the next answer reads again"
    );
    r.measured(3, Ok(true));
    assert_eq!(r.get(3).unwrap().failure, None);
    assert!(
        r.failure_is_new(3, "no PID"),
        "a reading that found a figure ends it"
    );
    assert!(r.view(&[3, 4]).contains_key(&3));
    assert!(r.view(&[4]).is_empty(), "gone with its container");
    assert!(
        r.note(3).newly_loaded,
        "a generation that left is not loaded"
    );
}

/// At rest: claimed once, only before any answer, and dropped when a request
/// answered meanwhile. A loaded reading marks the generation loaded.
#[test]
fn the_reading_at_rest_is_taken_once_and_only_at_rest() {
    let r = Residency::default();
    let now = Instant::now();
    assert!(r.begin_at_rest(1));
    assert!(!r.begin_at_rest(1), "once");
    r.at_rest(1, AtRest::Bare(5), now);
    assert_eq!(r.get(1).unwrap().at_rest, AtRest::Bare(5));

    assert!(r.begin_at_rest(2));
    r.note(2);
    r.at_rest(2, AtRest::Bare(5), now);
    assert_eq!(r.get(2).unwrap().at_rest, AtRest::Unread, "overtaken");
    assert!(!r.begin_at_rest(2), "not after an answer");

    assert!(r.begin_at_rest(3));
    r.at_rest(3, AtRest::Loaded(9), now);
    assert!(r.loaded(3), "taken as loaded");
}

/// The sampler's bookkeeping: one per generation, its maximum per reset
/// epoch, a stretch counts only with an answer, and a figure settles after
/// `SETTLE_AFTER` calm stretches — counted again from zero after a rise or a
/// reset.
#[test]
fn the_sampler_settles_and_a_reset_drops_what_it_saw() {
    let r = Residency::default();
    assert!(r.begin_sampling(7));
    assert!(!r.begin_sampling(7), "one sampler per generation");
    r.sample(7, 0, 10);
    r.sample(7, 0, 30);
    r.sample(7, 0, 20);
    assert_eq!(r.sampled(7, 0), Some(30));
    assert_eq!(
        r.end_sampling(7, 0),
        Stretch {
            max: Some(30),
            answered: false,
            raised: false
        },
        "no answer: nothing counts"
    );

    for _ in 0..SETTLE_AFTER {
        assert!(!r.settled("tts", "k"));
        r.stretch_done("tts", "k", false);
    }
    assert!(r.settled("tts", "k"));
    r.stretch_done("tts", "k", true);
    assert_eq!(r.calm("tts", "k"), 0, "a rise starts the count again");

    r.stretch_done("tts", "k", false);
    r.reset("tts");
    assert_eq!(r.calm("tts", "k"), 0);
    assert_eq!(r.epoch("tts"), 1);
    assert_eq!(
        r.sampled(7, 1),
        None,
        "what it saw before the reset is gone"
    );
}

/// The CPU switch: a row that inherits the class backend keeps the key it
/// had before rows could set one — pinned as hex, so every learned figure
/// survives the upgrade — and a row switched to the CPU and back is charged
/// its old figure again.
#[test]
fn an_inheriting_row_keeps_its_key_and_a_round_trip_through_the_cpu_finds_it_again() {
    let s = AudioSettings::default();
    let gpu = learned_row(3 * GIB);
    assert_eq!(resident_key(&row(), &s), "6d2a944225ef");
    let mut cpu = gpu.clone();
    cpu.backend = Some("cpu".into());
    assert_ne!(resident_key(&cpu, &s), resident_key(&gpu, &s));
    assert_eq!(learned(&cpu, &s), None, "nothing is charged on the CPU");
    cpu.backend = None;
    assert_eq!(learned(&cpu, &s), Some(3 * GIB));
    // Threads are no part of it: they change nothing that is loaded.
    cpu.threads = Some(8);
    assert_eq!(resident_key(&cpu, &s), resident_key(&gpu, &s));
}
