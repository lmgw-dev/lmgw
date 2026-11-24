//! The headroom per card (device-ledger design §9.1, WP13): the floor, the
//! learned room on top, the fixed value, an amount declared per card, the
//! margin of a model that is starting or not measured yet, and what
//! Settings says of each.

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

/// A starting model gets its class's margin, in every mode and from the
/// first moment (WP13); one that is measured — an audio row with its
/// residency learned, an image row whose context is measured — gets none.
#[test]
fn the_margin_is_the_class_gap_and_drops_once_measured() {
    let v = VramScheduler::new();
    let mut s = snap(HeadroomMode::Fixed);
    let fp = Footprint::new(2 * GIB, 0, None, None);
    assert_eq!(v.margin(&s, Class::Chat, "m", &fp), LLAMA_MARGIN);
    assert_eq!(
        v.start_headroom(&s, Class::Chat, "m", &fp),
        GIB + LLAMA_MARGIN,
        "fixed: the value and the margin on top"
    );
    s.settings.vram.headroom_mode = HeadroomMode::Auto;
    assert_eq!(
        v.start_margin(&s, Class::Chat, "m", &fp),
        GIB + LLAMA_MARGIN,
        "auto without readings: the whole floor and the margin"
    );
    assert_eq!(
        v.start_floor(&s, &fp),
        GIB,
        "the margin given up last: the floor alone"
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

/// A probe with per-process figures: auto can learn on it.
struct Measurable;

impl crate::vram::GpuProbe for Measurable {
    fn devices(&self) -> Result<Vec<crate::vram::GpuMemory>, String> {
        Ok(vec![])
    }

    fn source(&self) -> String {
        "measurable".into()
    }

    fn process_support(&self) -> Result<(), String> {
        Ok(())
    }
}

/// `n` cards listed by a device read, as the ledger records them, on a
/// probe with per-process figures; their ids.
fn machine(v: &VramScheduler, n: u32) -> Vec<DeviceId> {
    v.set_probe(std::sync::Arc::new(Measurable));
    let devices: Vec<crate::vram::GpuMemory> = (0..n)
        .map(|i| crate::vram::GpuMemory {
            id: DeviceId::new(format!("GPU-{i}")),
            index: i,
            name: "RTX 6000 Ada".into(),
            total_bytes: 48 * GIB,
            used_bytes: 0,
            free_bytes: 48 * GIB,
        })
        .collect();
    v.growth.lock().unwrap().set_present(&devices);
    devices.into_iter().map(|d| d.id).collect()
}

/// Readings on card `id`, the newest last: one per hour, ending this hour.
fn readings(v: &VramScheduler, id: &DeviceId, levels: &[u64]) {
    let now = growth::now_hour();
    let mut g = v.growth.lock().unwrap();
    let n = levels.len() as i64;
    for (i, b) in levels.iter().enumerate() {
        g.record(id, now - (n - 1 - i as i64), *b);
    }
}

fn chat() -> (Class, &'static str, Footprint) {
    (Class::Chat, "m", Footprint::new(2 * GIB, 0, None, None))
}

/// A card nobody else uses keeps nothing, a display server's few MiB do
/// not cost the 1 GiB floor, and a shared card keeps the floor from its
/// first reading. A card without readings keeps the whole floor.
#[test]
fn the_floor_is_capped_at_the_most_other_programs_held() {
    let v = VramScheduler::new();
    let ids = machine(&v, 4);
    readings(&v, &ids[0], &[0, 0]);
    readings(&v, &ids[1], &[4 * MIB, 6 * MIB, 5 * MIB]);
    readings(&v, &ids[2], &[3 * GIB]);
    let s = snap(HeadroomMode::Auto);
    let cards = v.cards_headroom(&s);
    let parts: Vec<(u64, u64, u64)> = cards
        .iter()
        .map(|c| (c.floor, c.room, c.structural))
        .collect();
    assert_eq!(
        parts,
        vec![
            (0, 0, 0),
            (6 * MIB, MIB, 4 * MIB + 6 * MIB),
            (GIB, 0, 3 * GIB + GIB),
            (GIB, 0, GIB),
        ],
        "nobody else; a display server; shared from its first reading; no readings"
    );
    let (class, m, fp) = chat();
    assert_eq!(
        v.start_margin(&s, class, m, &fp),
        6 * MIB + 2 * GIB + LLAMA_MARGIN,
        "the floors and the margin once"
    );
    let now = v.headroom_now(&s);
    assert!(
        now.cards[0]
            .sentence
            .starts_with("Nothing is always kept free: no other program has used this card"),
        "{}",
        now.cards[0].sentence
    );
    assert!(
        now.cards[1]
            .sentence
            .starts_with("6.0 MiB is always kept free for other programs, the most they held here"),
        "{}",
        now.cards[1].sentence
    );
    assert!(
        now.cards[2].sentence.starts_with(
            "1.0 GiB is always kept free for other programs. They have used up to 3.0 GiB here ("
        ) && now.cards[2]
            .sentence
            .ends_with("and use 3.0 GiB now: nothing more is kept free."),
        "{}",
        now.cards[2].sentence
    );
    assert_eq!(
        now.cards[3].sentence,
        "1.0 GiB is always kept free for other programs; no readings here yet."
    );
}

/// A few MiB of learned room is left out of the card's sentence (the owner's ruling,
/// 2026-11-18: a sentence about a few MiB of learned room is noise); it is
/// still kept.
#[test]
fn a_tiny_learned_room_is_left_out_of_the_sentence() {
    let v = VramScheduler::new();
    let ids = machine(&v, 1);
    readings(&v, &ids[0], &[2 * GIB, 2 * GIB + 11 * MIB, 2 * GIB]);
    let s = snap(HeadroomMode::Auto);
    assert_eq!(v.cards_headroom(&s)[0].room, 11 * MIB, "still kept");
    let sentence = &v.headroom_now(&s).cards[0].sentence;
    assert!(
        !sentence.contains("lmgw keeps the other") && sentence.ends_with("now."),
        "{sentence}"
    );
}

/// Outside use below its 7-day most: the learned room on top of the floor
/// is the most less what they hold now, kept while a start can make room
/// another way; the too-large test reads the floor and what they hold at
/// rest, never the room nor the margin (owner, 2026-11-17). The sentence
/// reads as the design's.
#[test]
fn the_learned_room_is_the_most_less_what_they_hold_now() {
    let v = VramScheduler::new();
    let ids = machine(&v, 1);
    let most = 41 * GIB + 205 * MIB;
    let now_b = 21 * GIB + 922 * MIB;
    readings(&v, &ids[0], &[2 * GIB, most, 12 * GIB, now_b]);
    let s = snap(HeadroomMode::Auto);
    let c = &v.cards_headroom(&s)[0];
    assert_eq!((c.floor, c.room), (GIB, most - now_b));
    let (class, m, fp) = chat();
    assert_eq!(
        v.start_headroom(&s, class, m, &fp),
        GIB + most - now_b + LLAMA_MARGIN
    );
    assert_eq!(v.start_margin(&s, class, m, &fp), GIB + LLAMA_MARGIN);
    assert_eq!(
        v.card_headroom(&s, &fp),
        2 * GIB + GIB,
        "at rest and the floor, never the room nor the margin"
    );
    let said = v.headroom_now(&s);
    let card = &said.cards[0];
    assert_eq!(card.room_bytes, most - now_b);
    assert_eq!(card.most_bytes, Some(most));
    let at = card.most_at.clone().unwrap();
    assert_eq!(
        card.sentence,
        format!(
            "1.0 GiB is always kept free for other programs. They have used up to 41.2 GiB here \
             ({at}) and use 21.9 GiB now: lmgw keeps the other 19.3 GiB free while it can make \
             room another way."
        )
    );
    for part in [
        "Headroom: auto, from what other programs did on the card: a floor of 1.0 GiB",
        "lmgw gives the learned room up last",
        "never refuses a model that fits beside the floor",
        "20.3 GiB kept free now, 19.3 GiB of it learned room",
        "a margin on top, in every mode, given up last with the learned room, where nothing \
         else fits beside the floor: 327 MiB for a llama.cpp model",
        "1.0 GiB for an image model",
        "at least 799 MiB for a voice model",
    ] {
        assert!(
            said.sentence.contains(part),
            "{part:?} in {}",
            said.sentence
        );
    }
    // The room is given up once nothing of lmgw's is left: warned with the
    // room and where they held their most.
    let t = crate::vram::Target {
        class,
        model_id: m.into(),
    };
    let kept = (
        v.start_headroom(&s, class, m, &fp),
        v.start_margin(&s, class, m, &fp),
        v.start_floor(&s, &fp),
    );
    let note = v
        .growth_let_go(&t, 2 * GIB, kept, 4 * GIB)
        .expect("the room was let go");
    assert!(
        note.starts_with(GROWTH_LET_GO)
            && note.contains("used up to 19.3 GiB more GPU memory than they hold now")
            && note.contains("they held 41.2 GiB on ")
            && note.contains("with the 1.0 GiB always kept free and its 327 MiB margin")
            && note.contains("(2.0 GiB for it, 4.0 GiB free), so it starts without that room:"),
        "{note}"
    );
    assert_eq!(v.growth_let_go(&t, 2 * GIB, kept, 30 * GIB), None, "kept");
    // Its margin given up too, where it fits beside the floor alone: said
    // plainly.
    let note = v
        .growth_let_go(&t, 2 * GIB, kept, 3 * GIB + 100 * MIB)
        .expect("the room and the margin were let go");
    assert!(
        note.starts_with(GROWTH_LET_GO)
            && note.contains("on top a margin of 327 MiB for what the start's estimate may")
            && note.contains("not with that room or that margin (2.0 GiB for it, 3.1 GiB free)")
            && note.contains("so it starts without either")
            && note.contains("or it take more than its estimate"),
        "{note}"
    );
    // Forgotten: the whole floor and no room until the next reading.
    v.growth.lock().unwrap().forget(None);
    assert_eq!(v.headroom(&s).bytes, GIB);
}

/// In `fixed` mode, `headroom_mb` is kept free on every card and the
/// margin rides on top; Settings says so.
#[test]
fn the_fixed_value_is_kept_on_every_card() {
    let v = VramScheduler::new();
    let ids = machine(&v, 4);
    readings(&v, &ids[2], &[10 * GIB, 13 * GIB, 10 * GIB]);
    let fixed = snap(HeadroomMode::Fixed);
    assert_eq!(v.headroom(&fixed).bytes, 4 * GIB);
    let (class, m, fp) = chat();
    assert_eq!(
        v.start_margin(&fixed, class, m, &fp),
        4 * GIB + LLAMA_MARGIN
    );
    assert_eq!(v.card_headroom(&fixed, &fp), 4 * GIB, "no margin");
    // No learned room in fixed mode: only the margin can be given up, and
    // the warning says so, and that an estimate that runs low comes out of
    // the value kept free.
    let t = crate::vram::Target {
        class,
        model_id: m.into(),
    };
    let one = (GIB + LLAMA_MARGIN, GIB + LLAMA_MARGIN, GIB);
    assert_eq!(
        v.growth_let_go(&t, 2 * GIB, one, 4 * GIB),
        None,
        "the margin kept"
    );
    let note = v
        .growth_let_go(&t, 2 * GIB, one, 3 * GIB + 100 * MIB)
        .expect("the margin was let go");
    assert!(
        note.starts_with(MARGIN_LET_GO)
            && note.contains("keeps a margin of 327 MiB beside chat/m")
            && note.contains("with the 1.0 GiB always kept free, not with that margin")
            && note.contains("comes out of the 1.0 GiB kept free for other programs"),
        "{note}"
    );
    let nothing = (LLAMA_MARGIN, LLAMA_MARGIN, 0);
    let note = v
        .growth_let_go(&t, 2 * GIB, nothing, 2 * GIB + 100 * MIB)
        .expect("the margin was let go on a card that is lmgw's");
    assert!(
        note.contains("with nothing kept free for other programs here")
            && note.contains("the GPU can run out of memory"),
        "{note}"
    );
    let said = v.headroom_now(&fixed);
    assert!(
        said.sentence.starts_with(
            "Headroom: fixed, 1.0 GiB kept free for other programs on each of the 4 graphics \
             cards. 4.0 GiB kept free on the 4 cards together now."
        ),
        "{}",
        said.sentence
    );
    assert!(
        said.cards[2]
            .sentence
            .starts_with("1.0 GiB is always kept free for other programs (fixed headroom). They"),
        "{}",
        said.cards[2].sentence
    );
    assert_eq!(said.cards[2].suggest, None, "no suggestion outside auto");
    // One card: as before cards.
    let one = VramScheduler::new();
    machine(&one, 1);
    assert_eq!(one.headroom(&fixed).bytes, GIB);
    assert!(one
        .headroom_now(&fixed)
        .sentence
        .starts_with("Headroom: fixed, 1.0 GiB kept free for other programs. A model that is"));
}

/// An amount declared per card (WP13 C): "always" N is the card's floor,
/// with no learned room on top, in the too-large test too, and never given
/// up; "nothing" keeps neither, and the margin still rides until it is
/// given up last. The other cards are as before.
#[test]
fn an_amount_declared_per_card_is_kept_as_set() {
    let v = VramScheduler::new();
    let ids = machine(&v, 3);
    for id in &ids {
        readings(&v, id, &[GIB, 5 * GIB, 2 * GIB]);
    }
    let mut s = snap(HeadroomMode::Auto);
    s.settings.vram.keep_free.insert(
        ids[0].clone(),
        KeepFree {
            mode: KeepFreeMode::Always,
            mb: 4096,
        },
    );
    s.settings.vram.keep_free.insert(
        ids[1].clone(),
        KeepFree {
            mode: KeepFreeMode::Nothing,
            mb: 0,
        },
    );
    let parts: Vec<(u64, u64, u64)> = v
        .cards_headroom(&s)
        .iter()
        .map(|c| (c.floor, c.room, c.structural))
        .collect();
    assert_eq!(
        parts,
        vec![(4 * GIB, 0, 4 * GIB), (0, 0, 0), (GIB, 3 * GIB, GIB + GIB)]
    );
    let t = crate::vram::Target {
        class: Class::Chat,
        model_id: "m".into(),
    };
    let (_, m, fp) = chat();
    let heads = v.cards_headroom(&s);
    let rules = |id: &DeviceId| {
        let r = v.rules_on(&s, &t, &fp, &heads, id);
        (r.headroom, r.margin, r.floor, r.structural)
    };
    assert_eq!(
        rules(&ids[1]),
        (LLAMA_MARGIN, LLAMA_MARGIN, 0, 0,),
        "nothing: the margin alone, given up last"
    );
    assert_eq!(
        rules(&ids[0]),
        (
            4 * GIB + LLAMA_MARGIN,
            4 * GIB + LLAMA_MARGIN,
            4 * GIB,
            4 * GIB,
        ),
        "always: the amount never given up, the margin given up last"
    );
    assert_eq!(
        rules(&ids[2]),
        (
            GIB + 3 * GIB + LLAMA_MARGIN,
            GIB + LLAMA_MARGIN,
            GIB,
            GIB + GIB,
        ),
        "automatic: the room, then the margin, given up; the floor never"
    );
    let _ = m;
    let said = v.headroom_now(&s);
    assert!(
        said.cards[0].sentence.starts_with(
            "4.0 GiB is always kept free for other programs, as set for this card: a model that \
             would need it waits, goes on another card or falls back."
        ),
        "{}",
        said.cards[0].sentence
    );
    assert!(said.cards[1]
        .sentence
        .starts_with("Nothing is kept free for other programs: this card is lmgw's, as set."));
    assert!(
        said.sentence.contains(
            "RTX 6000 Ada (card 0) and RTX 6000 Ada (card 1) keep the amount set for them."
        ),
        "{}",
        said.sentence
    );
    assert_eq!(said.cards[0].keep.mb, 4096);
    assert_eq!(said.cards[0].suggest, None);
    assert_eq!(said.cards[1].suggest, None);
}

/// Settings suggests an amount from what was seen: the floor and the most
/// other programs used beyond what they hold now (owner, 2026-11-17:
/// "always" replaces the floor, so accepting it never lowers what the card
/// keeps), and the separate times they used more than that and the floor
/// together; none where they never did.
#[test]
fn settings_suggests_an_amount_from_what_was_seen() {
    let v = VramScheduler::new();
    let ids = machine(&v, 2);
    let up = 4 * GIB + 100 * MIB;
    readings(
        &v,
        &ids[0],
        &[GIB, GIB + up, GIB, 3 * GIB, 3 * GIB, GIB, 4 * GIB, GIB],
    );
    readings(&v, &ids[1], &[GIB, GIB + 512 * MIB, GIB]);
    let said = v.headroom_now(&snap(HeadroomMode::Auto));
    let sug = said.cards[0].suggest.clone().expect("a suggestion");
    assert_eq!(sug.mb, (GIB + up) / MIB, "the floor and the room");
    assert_eq!(sug.times, 3, "three separate times above 2 GiB");
    assert_eq!(
        sug.sentence,
        "Other programs used up to 4.1 GiB more than now, 3 times in 7 days: always keep 5.1 GiB \
         free (the 1.0 GiB floor and that 4.1 GiB)?"
    );
    let card = &said.cards[0];
    assert_eq!(
        sug.mb * MIB,
        card.floor_bytes + card.room_bytes,
        "per card, once"
    );
    assert_eq!(
        said.cards[1].suggest, None,
        "never more than now and the floor together"
    );
}

/// The settings merge: only the cards given change, `automatic` takes an
/// amount out, `always` needs at least 1 MiB, and a card that is neither
/// listed nor has readings is refused, with nothing changed.
#[test]
fn an_amount_is_set_per_card_and_refused_for_an_unknown_card() {
    let v = VramScheduler::new();
    let ids = machine(&v, 2);
    let always = |mb| KeepFree {
        mode: KeepFreeMode::Always,
        mb,
    };
    let mut kept = std::collections::BTreeMap::new();
    v.merge_keep_free(&mut kept, [(ids[0].clone(), always(2048))].into())
        .unwrap();
    v.merge_keep_free(
        &mut kept,
        [(
            ids[1].clone(),
            KeepFree {
                mode: KeepFreeMode::Nothing,
                mb: 7,
            },
        )]
        .into(),
    )
    .unwrap();
    assert_eq!(kept[&ids[0]], always(2048), "untouched by the second");
    assert_eq!(kept[&ids[1]].mb, 0, "nothing keeps no amount");
    let err = v
        .merge_keep_free(&mut kept, [(ids[0].clone(), always(0))].into())
        .unwrap_err();
    assert!(err.contains("at least 1 MiB"), "{err}");
    let err = v
        .merge_keep_free(
            &mut kept,
            [
                (ids[0].clone(), KeepFree::default()),
                (DeviceId::new("GPU-gone"), always(1)),
            ]
            .into(),
        )
        .unwrap_err();
    assert!(
        err.contains("GPU-gone") && err.contains("RTX 6000 Ada (card 0) (GPU-0)"),
        "{err}"
    );
    assert!(kept.contains_key(&ids[0]), "nothing changed");
    v.merge_keep_free(&mut kept, [(ids[0].clone(), KeepFree::default())].into())
        .unwrap();
    assert!(!kept.contains_key(&ids[0]), "automatic takes it out");
}

/// A machine without per-process figures: auto keeps the whole fixed value,
/// and says why.
#[test]
fn without_per_process_figures_auto_keeps_the_fixed_value() {
    let v = VramScheduler::new();
    v.set_probe(std::sync::Arc::new(NoTelemetry("no driver".into())));
    let s = snap(HeadroomMode::Auto);
    assert_eq!(v.headroom(&s).bytes, GIB);
    let said = v.headroom_now(&s);
    assert!(
        said.sentence
            .contains("this machine gives no per-program GPU memory figures"),
        "{}",
        said.sentence
    );
}

/// The audit line of a forget names the card by name and index with its id,
/// and the figure that was dropped.
#[test]
fn the_audit_line_names_the_card_and_what_was_dropped() {
    let one = Forgotten {
        id: DeviceId::new("GPU-2"),
        label: "RTX PRO 6000 Blackwell (card 2)".into(),
        most: Some(3 * GIB),
        hours: 30,
        room: GIB,
    };
    let t = audit_target(Some(&one.id), std::slice::from_ref(&one));
    assert_eq!(t["card"], "RTX PRO 6000 Blackwell (card 2)");
    assert_eq!(t["card_id"], "GPU-2");
    assert_eq!(
        t["dropped"],
        "readings of other programs up to 3.0 GiB, from 30 hours, with 1.0 GiB of learned room \
         kept free"
    );
    let all = audit_target(None, &[one]);
    assert_eq!(all["card"], "every card");
    assert!(
        all["dropped"]
            .as_str()
            .unwrap()
            .starts_with("RTX PRO 6000 Blackwell (card 2) (GPU-2): readings of other programs"),
        "{all:?}"
    );
}
