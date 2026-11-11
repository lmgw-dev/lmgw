//! A llama-server row that llama.cpp's `--fit` would shrink: an unset
//! context, `--n-gpu-layers` on auto (`vram::plan::chat::fit`), under the owner's ruling of 2026-11-11 (never block a start that would work; warn instead). Only where its image's llama-server has `--fit`
//! (`vram::fit_vocab`); anywhere else it is charged as written.
//!
//! **What it is charged.** Never the unshrinkable whole where that cannot
//! fit: where what the card can give it now — its capacity less what other
//! programs hold there now, lmgw's own models counted as room, since a wait
//! or an eviction frees them — cannot hold the row as written beside its
//! margin (a trained context of 262144 tokens on a 4 GiB card), the row is
//! charged its **target**: the most of it that fits there, found the way
//! `--fit` finds it ([`fit_to_card`]). Each decision fits it again on its
//! own reading of the card (`VramScheduler::fit_in`), the same figures its
//! admission check compares, so a target fitted on a card that holds only
//! other programs' memory is admitted. The learned growth never shrinks it:
//! it only makes the start wait for, or evict, lmgw's own models, and once
//! none are left the start goes ahead without it, warned (`headroom`). The
//! target is what the card can give it at all now, so it goes ahead, a
//! fallback or not ([`whole`]).
//!
//! **Less than the target** ([`decide`]) lasts for the container's life, so
//! only where it is the only way the model runs now: nothing of lmgw's to
//! wait for and no fallback that would answer. Then a lower context first,
//! layers off the card after. It waits, as any model does, while not even
//! its least placement fits what is free; it is refused, with numbers, only
//! where its least does not fit the card beside what other programs hold at
//! rest and its margin (`VramScheduler::card_headroom`) — never because of
//! the learned growth or of what other programs hold now. A fallback
//! answers in its place only where its least does not fit beside what other
//! programs hold now (`partial::least`, §4.7's verdict).
//!
//! **What llama-server is told.** A placement below the row as written is
//! rendered as explicit `--ctx-size`, `--n-cpu-moe` and `--n-gpu-layers`
//! with `--fit off` (`runtime::argv::llama_fit`), so `--fit` cannot size the
//! model to the driver's free memory, which includes what admission keeps
//! free for others; the container records it (`lmgw.llama-fit`) so boot
//! adopts it. A row the card holds as written gets nothing extra. A row that
//! sets its context or layers itself is held to them as written, and its
//! own `--fit-target` is kept free beside it ([`keep`]).
//!
//! **Said.** A lowered context ("Context fitted to the GPU: …") and layers
//! off the card ([`super::PARTIAL_FIT`]) are logged and kept on the entry
//! with both figures, what is free and the flags it runs with.
//!
//! **Not yet.** Several cards: llama.cpp splits layers by each card's free
//! memory while the ledger pools them, so a row whose engine sees more than
//! one card is charged as written; the target is the entry's for its life,
//! as an image grant is.

use crate::hf::fmt_bytes;
use crate::runtime::registry::{LlamaPlacement, PartialGrant};

use super::super::plan::chat::fit::LlamaFit;
use super::super::{Footprint, Target};
use super::{Grant, Room, PARTIAL_FIT};

/// The lead of a start whose context lmgw lowered, all of it on the card.
pub const CONTEXT_FITTED: &str = "Context fitted to the GPU";

/// The card a row is fitted to ([`fit_to_card`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Card {
    /// Its capacity.
    pub total: u64,
    /// What it can give this start now: the capacity less what other
    /// programs hold on it now, lmgw's own models counted as room (a wait
    /// or an eviction frees them). A decision reads it off its ledger
    /// (`VramScheduler::fit_in`), the same figures its admission check
    /// compares, so a target fitted on a card that holds only other
    /// programs' memory is admitted.
    pub avail: u64,
    /// What it needs beside it whatever other programs do next
    /// (`VramScheduler::start_margin`): its margin, never the learned
    /// growth, which decides waiting and never shrinks the target.
    pub margin: u64,
    /// What it needs beside it on an otherwise empty card
    /// (`VramScheduler::card_headroom`): whether it can run at all is
    /// judged against this, never against the learned growth.
    pub structural: u64,
}

/// `fp` fitted to `card`, for a start whose engine sees `cards` GPUs: a row
/// llama.cpp's `--fit` would shrink is charged the most of it that fits in
/// what the card can give it now beside its margin (module doc), and
/// carries that placement as its target; where not even its least fits
/// that, its least, which waits for room. Rows without a sizer, or on
/// other than exactly one card, keep the whole; one whose least does not
/// fit the card beside what it needs there at all is too large, as any
/// model is.
pub fn fit_to_card(mut fp: Footprint, card: Card, cards: Option<usize>) -> Footprint {
    let Some(fit) = fp.llama.take() else {
        return fp;
    };
    if cards != Some(1) {
        // Several cards: as written (module doc).
        return fp;
    }
    // Fitted before (on another reading of the card): from the row as
    // written again.
    if !fit.target.is_whole() {
        fp = fit.sizer.footprint(&LlamaPlacement::default());
    }
    let least = fit.sizer.minimum();
    if fit.sizer.charge(&least) > card.total.saturating_sub(keep(&fit, card.structural)) {
        fp.llama = Some(LlamaFit {
            target: LlamaPlacement::default(),
            fits_card: false,
            ..fit
        });
        return fp;
    }
    let kept = keep(&fit, card.margin);
    let room = card.avail.min(card.total).saturating_sub(kept);
    let whole = fit.sizer.charge(&LlamaPlacement::default());
    let target = fit.sizer.best(room).unwrap_or(least);
    if target.is_whole() {
        fp.llama = Some(LlamaFit {
            target,
            fits_card: true,
            ..fit
        });
        return fp;
    }
    let sizer = fit.sizer.clone();
    let mut placed = sizer.footprint(&target);
    placed.note = Some(format!(
        "{}{}llama.cpp's --fit would shrink it to the driver's free memory; it needs {} as \
         written, and the most of it that fits in the {} the card's {} can give it now (less \
         what other programs hold there) beside the {} {} is {} with {}",
        placed.note.as_deref().unwrap_or_default(),
        if placed.note.is_some() { ". " } else { "" },
        fmt_bytes(whole),
        fmt_bytes(card.avail.min(card.total)),
        fmt_bytes(card.total),
        fmt_bytes(kept),
        kept_as(&fit, card.margin),
        fmt_bytes(placed.total_bytes),
        target.flags()
    ));
    placed.llama = Some(LlamaFit {
        sizer,
        target,
        fits_card: true,
    });
    placed
}

/// What is kept free beside a fitted row in place of `headroom`: the
/// row's own `--fit-target` where that is more. `--fit off` leaves
/// llama.cpp nothing to keep it with, so admission keeps it instead.
fn keep(l: &LlamaFit, headroom: u64) -> u64 {
    l.sizer.fit_target.map_or(headroom, |t| t.max(headroom))
}

/// How [`keep`]'s figure reads in a sentence.
fn kept_as(l: &LlamaFit, margin: u64) -> &'static str {
    match l.sizer.fit_target {
        Some(t) if t > margin => "its own --fit-target keeps free",
        _ => "margin kept free beside it",
    }
}

/// The least `fp` runs with on the card, in bytes, where its engine sees
/// `cards` GPUs (`partial::least`): its least placement for a row fitted to
/// one card, else all of it.
pub(super) fn least_bytes(fp: &Footprint, cards: Option<usize>) -> u64 {
    match fp.llama.as_ref() {
        Some(l) if l.fits_card && cards == Some(1) => {
            l.sizer.charge(&l.sizer.minimum()).min(fp.total_bytes)
        }
        _ => fp.total_bytes,
    }
}

/// Whether `fp` can run with less than its charge (module doc): it has a
/// sizer whose least fits the card.
pub(super) fn adapts(fp: &Footprint) -> bool {
    fp.llama.as_ref().is_some_and(|l| l.fits_card)
}

/// The grant of a start that got its whole charge: its target rendered,
/// with the lowered context said. `None` for a row whose target is the row
/// as written: nothing is rendered over it, and it starts as it always did
/// (llama.cpp's own `--fit` included, module doc).
///
/// The target is what this card holds of the row at all, beside the
/// headroom: no wait and no eviction makes more of it fit, so it goes ahead
/// as an image pipeline's on-card part does, a fallback or not. Only less
/// than the target ([`decide`]) waits for lmgw's own models or leaves the
/// request to a fallback.
pub(super) fn whole(target: &Target, fp: &Footprint, room: Room) -> Option<Grant> {
    let l = fp.llama.as_ref()?;
    if l.target.is_whole() {
        return None;
    }
    let warning = match l.target.ctx {
        Some(c) if l.target.n_gpu_layers.is_none() && l.target.n_cpu_moe.is_none() => {
            context_note(target, fp, l, room, c, fp.total_bytes)
        }
        _ => layers_note(target, fp, l, room, &l.target, fp.total_bytes),
    };
    Some(Grant::Partial(PartialGrant {
        bytes: fp.total_bytes,
        max_vram_bytes: 0,
        warning,
        llama: Some(l.target),
    }))
}

/// What `fp` is given once nothing more can be made free (module doc), or
/// `None` for the normal path.
pub(super) fn decide(target: &Target, fp: &Footprint, room: Room) -> Option<Grant> {
    let l = fp.llama.as_ref().filter(|l| l.fits_card)?;
    if room.cards != Some(1) {
        return None;
    }
    // Less than its target is for the container's whole life, so only where
    // it is the only way the model runs now: a configured fallback answers
    // instead (it always applies), and lmgw's own models on the card are
    // waited for — they finish, drain or are evicted — as for any start.
    if room.fallback || room.others_hold {
        return None;
    }
    let avail = room.free.saturating_sub(keep(l, room.headroom));
    let p = l.sizer.best(avail).filter(|p| !p.is_whole())?;
    let layers_off = p.n_gpu_layers.is_some() || p.n_cpu_moe.is_some();
    let bytes = l.sizer.charge(&p);
    let warning = match p.ctx {
        Some(c) if !layers_off => context_note(target, fp, l, room, c, bytes),
        _ => layers_note(target, fp, l, room, &p, bytes),
    };
    Some(Grant::Partial(PartialGrant {
        bytes,
        max_vram_bytes: 0,
        warning,
        llama: Some(p),
    }))
}

fn context_note(
    target: &Target,
    fp: &Footprint,
    l: &LlamaFit,
    room: Room,
    ctx: u64,
    bytes: u64,
) -> String {
    format!(
        "{CONTEXT_FITTED}: {}/{} needs {} with its context of {} tokens plus {} headroom and {} \
         is free; llama.cpp's --fit would lower its context to what the driver shows free, so \
         VRAM admission gives it {} on the card, all of its layers there, and tells \
         llama-server --ctx-size {ctx} --fit off (never below --fit-ctx {})",
        target.class.as_str(),
        target.model_id,
        fmt_bytes(whole_bytes(fp, l)),
        l.sizer.trained.unwrap_or(0),
        fmt_bytes(room.headroom),
        fmt_bytes(room.free),
        fmt_bytes(bytes),
        l.sizer.min_ctx,
    )
}

fn layers_note(
    target: &Target,
    fp: &Footprint,
    l: &LlamaFit,
    room: Room,
    p: &LlamaPlacement,
    bytes: u64,
) -> String {
    let layers = match (p.n_gpu_layers, p.n_cpu_moe) {
        (Some(n), _) => format!(
            "{} of its {} layers and the output layer on the card{}",
            n.saturating_sub(1),
            l.sizer.layers,
            if p.n_cpu_moe.is_some() {
                ", every routed expert in host memory"
            } else {
                ""
            }
        ),
        (None, Some(k)) => format!(
            "the routed experts of its first {k} of {} layers in host memory, the rest on the \
             card",
            l.sizer.layers
        ),
        (None, None) => "all of its layers on the card".into(),
    };
    format!(
        "{PARTIAL_FIT}: {}/{} needs {} in full plus {} headroom and {} is free; llama.cpp's \
         --fit would move layers off the card to fit the driver's free memory. VRAM admission \
         gives it {} on the card, {layers}, and tells llama-server {}",
        target.class.as_str(),
        target.model_id,
        fmt_bytes(whole_bytes(fp, l)),
        fmt_bytes(room.headroom),
        fmt_bytes(room.free),
        fmt_bytes(bytes),
        p.flags(),
    )
}

/// The row as written: every layer at its unshrunk context.
fn whole_bytes(fp: &Footprint, l: &LlamaFit) -> u64 {
    if l.target == LlamaPlacement::default() {
        fp.total_bytes
    } else {
        l.sizer.charge(&LlamaPlacement::default())
    }
}

/// Why `fp` is not given anything in `room`, for a refusal or a wait's log
/// line: `None` for a row `--fit` would not shrink.
pub(super) fn why_not(fp: &Footprint, room: Room) -> Option<String> {
    let l = fp.llama.as_ref()?;
    let least = l.sizer.minimum();
    let least_bytes = l.sizer.charge(&least);
    let avail = room.free.saturating_sub(keep(l, room.headroom));
    Some(match room.cards {
        Some(1) => match l.sizer.best(avail) {
            Some(p) if room.fallback => format!(
                "llama.cpp's --fit would run it with less than its {} ({}, {}), but a fallback \
                 answers in its place",
                fmt_bytes(fp.total_bytes),
                p.flags(),
                fmt_bytes(l.sizer.charge(&p))
            ),
            Some(p) if room.others_hold => format!(
                "llama.cpp's --fit would run it with less than its {} ({}, {}) for as long as \
                 its container lives, so it waits for lmgw's own models on the card instead",
                fmt_bytes(fp.total_bytes),
                p.flags(),
                fmt_bytes(l.sizer.charge(&p))
            ),
            _ => format!(
                "even the least it runs with ({}) needs {} and {} is free beyond the headroom",
                least.flags(),
                fmt_bytes(least_bytes),
                fmt_bytes(avail)
            ),
        },
        Some(n) => format!(
            "llama.cpp's --fit could run it with less on the card, but its engine would see {n} \
             cards and VRAM admission pools them, so fitting it on several cards is not done yet \
             and it needs all of it free"
        ),
        None => "llama.cpp's --fit could run it with less on the card, but how many cards its \
                 engine would see is not known (no device reading), so it needs all of it free"
            .into(),
    })
}

/// The end of a refusal for a row too large for the card: what its least
/// placement needs.
pub(super) fn too_large_hint(fp: &Footprint) -> String {
    let Some(l) = fp.llama.as_ref().filter(|l| !l.fits_card) else {
        return String::new();
    };
    let least = l.sizer.minimum();
    format!(
        ". Even the least llama.cpp's --fit would run it with ({}) needs {}",
        least.flags(),
        fmt_bytes(l.sizer.charge(&least))
    )
}

#[cfg(test)]
mod tests;
