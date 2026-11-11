//! What a row llama.cpp's `--fit` would shrink is fitted to and given: its
//! target against what the card can give it now beside its margin, less
//! only where nothing of lmgw's could be waited for and no fallback would
//! answer, never too large because of other programs' use now, and its own
//! `--fit-target` kept (llama-fit review findings 3, 5 and 6, and the
//! second review's finding A, 2026-11-11).

use std::sync::Arc;

use super::*;
use crate::config::LlamaParams;
use crate::runtime::Class;
use crate::vram::plan::chat::fit::tests::{dense, files, shape};
use crate::vram::plan::chat::fit::Sizer;
use crate::vram::plan::chat::footprint;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// The Qwen3-4B-shaped row with no context set, as the plan carries it.
fn row(args: &[&str]) -> Footprint {
    let f = files(dense());
    let s = shape(LlamaParams::default(), args);
    let fp = footprint(&f, "m.gguf", &s);
    Footprint {
        llama: Some(LlamaFit {
            sizer: Arc::new(Sizer::of(f, "m.gguf", &s).expect("--fit would shrink it")),
            target: LlamaPlacement::default(),
            fits_card: true,
        }),
        ..fp
    }
}

/// A card of `total` bytes with 256 MiB of other programs' memory on it
/// now, and `margin` kept beside a start.
fn card(total: u64, margin: u64) -> Card {
    Card {
        total,
        avail: total - 256 * MIB,
        margin,
        structural: margin,
    }
}

fn target() -> Target {
    Target {
        class: Class::Chat,
        model_id: "m".into(),
    }
}

fn room(free: u64, others_hold: bool, fallback: bool) -> Room {
    Room {
        free,
        headroom: GIB,
        others_hold,
        fallback,
        cards: Some(1),
    }
}

/// Fitted to what the card can give it now beside its margin, so a start
/// on a card that holds only other programs' memory gets all of its target.
#[test]
fn the_target_fits_what_the_card_can_give_now() {
    let c = card(8 * GIB, GIB);
    let fp = fit_to_card(row(&[]), c, Some(1));
    let l = fp.llama.as_ref().unwrap();
    assert!(l.fits_card && l.target.ctx.is_some(), "{l:?}");
    assert!(
        fp.total_bytes + GIB <= c.avail,
        "admitted where that much is free: {}",
        fp.total_bytes
    );
    assert!(
        fp.note.as_deref().unwrap().contains("can give it now"),
        "{:?}",
        fp.note
    );
    let Some(Grant::Partial(g)) = whole(&target(), &fp, room(c.avail, false, false)) else {
        panic!("the target, rendered");
    };
    assert_eq!(g.llama, Some(l.target));
}

/// Less than the target lasts for the container's life: never while a
/// fallback would answer or lmgw's own models could be waited for.
#[test]
fn less_than_the_target_only_when_it_is_the_only_way_now() {
    let fp = fit_to_card(row(&[]), card(8 * GIB, GIB), Some(1));
    let short = fp.total_bytes;
    assert!(decide(&target(), &fp, room(short, false, true)).is_none());
    assert!(decide(&target(), &fp, room(short, true, false)).is_none());
    let why = why_not(&fp, room(short, true, false)).unwrap();
    assert!(why.contains("waits for lmgw's own models"), "{why}");
    let why = why_not(&fp, room(short, false, true)).unwrap();
    assert!(why.contains("a fallback answers in its place"), "{why}");
    let Some(Grant::Partial(g)) = decide(&target(), &fp, room(short, false, false)) else {
        panic!("the only way it runs now");
    };
    assert!(
        g.bytes < short && g.warning.starts_with(CONTEXT_FITTED),
        "{g:?}"
    );
}

/// Other programs holding most of the card now only decide whether there
/// is room now: the row still fits the card (its least, waiting for room),
/// where the structural judgement — at rest and the margin — says so.
#[test]
fn other_programs_use_now_never_makes_it_too_large() {
    let busy = Card {
        total: 8 * GIB,
        avail: GIB,
        margin: GIB,
        structural: GIB,
    };
    let fp = fit_to_card(row(&[]), busy, Some(1));
    let l = fp.llama.as_ref().unwrap();
    assert!(l.fits_card, "not too large: {l:?}");
    assert_eq!(l.target, l.sizer.minimum(), "its least, waiting for room");
    assert!(adapts(&fp));
}

/// A row's own `--fit-target` is what `--fit off` would otherwise drop:
/// admission keeps it free instead, so the row gets less beside it.
#[test]
fn the_rows_own_fit_target_is_kept() {
    let plain = fit_to_card(row(&[]), card(8 * GIB, GIB), Some(1));
    let own = fit_to_card(row(&["-fitt", "3072"]), card(8 * GIB, GIB), Some(1));
    assert_eq!(own.llama.as_ref().unwrap().sizer.fit_target, Some(3 * GIB));
    assert!(
        own.total_bytes <= 8 * GIB - 3 * GIB - 256 * MIB,
        "{}",
        own.total_bytes
    );
    assert!(own.total_bytes < plain.total_bytes);
    assert!(
        own.note
            .as_deref()
            .unwrap()
            .contains("its own --fit-target keeps free"),
        "{:?}",
        own.note
    );
}

/// The row as written fits: nothing is rendered over it.
#[test]
fn a_row_the_card_holds_whole_gets_no_placement() {
    let fp = fit_to_card(row(&[]), card(64 * GIB, GIB), Some(1));
    assert!(fp.llama.as_ref().unwrap().target.is_whole());
    assert!(whole(&target(), &fp, room(60 * GIB, false, false)).is_none());
}
