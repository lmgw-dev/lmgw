//! A planned file's fit over the graphics cards, in signals and choices
//! (device-ledger design §13 WP12, the owner's decisions of 2026-11-17).
//!
//! Beside a fit on one card ([`super::fit`]) every fit carries a
//! [`CardsFit`]:
//! - **Min. cards**: the fewest cards whose total memory holds the file at
//!   its full context (the plan's), as admission's "too large" rule counts
//!   a card and as lmgw would split it (`vram::place::marks`). Hidden on a
//!   computer of one card; a declared GPU memory budget without a device
//!   reading says "budget" with a tick or a cross in its place (with one,
//!   the cards are read under the budget as admission reads them). A file
//!   that only a split holds and lmgw cannot split here — an aux model, which runs on one card, or an
//!   engine that cannot be told a split — says so and why.
//! - **Free VRAM**: whether the cards it would use have room now, as an
//!   owner's start decides it (`vram::place`), at the context Get it would
//!   create: now, after unloading lmgw's idle models (named), after busy
//!   ones finish, or what it needs against what is there.
//! - **With experts in RAM**: for a mixture-of-experts model no card holds
//!   at its full context, how many cards hold it with every layer's routed
//!   experts in the computer's memory (`--n-cpu-moe`).
//!
//! **The context** the signals are judged at is the one Get it would
//! create with the fit's measure: the fit's shorter one where it fits only
//! a shorter conversation on one card, else the full one. A file that one
//! card holds whole at its full context is set up as before. One that only
//! a split or its experts in RAM hold at its full context is where lmgw
//! cannot decide for the person: Get it asks (`choices`), with the numbers
//! of a split over the cards at the full context, of one card at the
//! shorter context, and of the experts in RAM where that applies. The full
//! context is then the one judged.

use std::sync::Arc;

use lmgw_api_types::model_find::{
    CardPart, CardsFit, ExpertsInRam, FileFit, FitMark, FreeState, FreeVram, GetChoice, GetLayout,
    MinCards, MinCardsState, Sign,
};
use serde_json::json;

use crate::hf::fmt_bytes;
use crate::vram::place::marks::{CardsNow, Free, Min};
use crate::vram::plan::chat::by_layer::ByLayer;

use super::fit::{CardNow, Sizer};
use super::pick::tokens;
use super::plan_files::PlanFiles;

/// Whether lmgw can split a file of this kind on these cards, and why not.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CanSplit {
    Yes,
    /// It can't: the sentence's end, after "fits only split; ".
    No(String),
}

/// The fit over the cards of a planned file (module doc): `sizer` is the
/// plan's, `fit` the one-card fit by the measure Get it would use. `None`
/// for a model planned on the CPU and one marked by its size.
pub fn judge(files: &PlanFiles, sizer: &Sizer, card: &CardNow, fit: &FileFit) -> Option<CardsFit> {
    let params = &sizer.params;
    if sizer.on_cpu || files.size_only.is_some() {
        return None;
    }
    let full = sizer.ctx;
    let at = |s: &Sizer, c: Option<u64>| match c {
        Some(c) => s.at(c),
        None => s.at_planned(),
    };
    let fp_full = at(sizer, full);
    let chat = sizer.is_chat();
    let cards = card.cards.as_deref();
    let of = cards.map_or(1, CardsNow::count);
    let can = can_split(chat, fp_full.by_layer.as_deref(), cards);
    let cost = |fp: &crate::vram::Footprint| match (&can, of > 1) {
        (CanSplit::Yes, true) => fp.by_layer.clone(),
        _ => None,
    };
    let mut out = CardsFit {
        ctx: full,
        of: u32::try_from(of).unwrap_or(u32::MAX),
        ..CardsFit::default()
    };

    // Min. cards at the full context.
    let min_full = cards.map(|c| c.min_cards(fp_full.total_bytes, fp_full.by_layer.as_deref()));
    let holds_one = match (&min_full, cards) {
        (Some(m), _) => matches!(m, Min::One(_)),
        // A budget, or one pooled card: the room of the whole card.
        (None, _) => card.card.fits(&fp_full),
    };
    out.min_cards = min_cards(card, cards, min_full.as_ref(), &can, &fp_full, full, fit);

    // With the experts in RAM, where no card holds it whole.
    let moe = chat
        && !holds_one
        && crate::vram::plan::chat::fit::has_experts(&files.summary)
        && files.summary.block_count.is_some_and(|n| n > 0);
    let experts = moe.then(|| {
        let mut p = params.clone();
        p.insert(
            "n_cpu_moe".into(),
            json!(files.summary.block_count.unwrap_or(0)),
        );
        let s = Sizer::chat(files, &p);
        let fp = at(&s, full);
        experts_in_ram(card, cards, &fp, &fp_full, cost(&fp), &can, full)
    });

    // Get it asks where only a split or the experts in RAM hold the full
    // context.
    let split_choice = match (&min_full, &can) {
        (Some(Min::Split(parts)), CanSplit::Yes) => {
            let free = free_vram(card, cards, &fp_full, cost(&fp_full), parts.len());
            Some(choice(GetLayout::Split, full, parts, 0, free))
        }
        _ => None,
    };
    let experts_choice = experts.as_ref().filter(|e| e.sign != Sign::No).map(|e| {
        choice(
            GetLayout::ExpertsInRam,
            full,
            &e.parts,
            e.ram_bytes,
            e.free.clone(),
        )
    });
    let asks = split_choice.is_some() || experts_choice.is_some();
    let judged = match (asks, fit.mark, fit.ctx) {
        (false, FitMark::FitsShorter, Some(c)) => Some(c),
        _ => full,
    };
    out.ctx = judged;
    let fp_judged = match judged == full {
        true => fp_full.clone(),
        false => at(sizer, judged),
    };
    out.free = match (&split_choice, &experts_choice) {
        (Some(c), _) | (None, Some(c)) => c.free.clone(),
        (None, None) => {
            let k = match &min_full {
                Some(Min::Split(p)) => p.len(),
                _ => 1,
            };
            free_vram(card, cards, &fp_judged, cost(&fp_judged), k)
        }
    };
    if asks {
        out.choices.extend(split_choice);
        out.choices.extend(experts_choice);
        if let (FitMark::FitsShorter, Some(c)) = (fit.mark, fit.ctx) {
            let fp = at(sizer, Some(c));
            let free = free_vram(card, cards, &fp, None, 1);
            let parts = free.cards.clone();
            out.choices
                .push(choice(GetLayout::OneCard, Some(c), &parts, 0, free));
        }
        out.notes.push(
            "No single card holds it at its full context: Get it asks how to set it up, and \
             shows each way with its numbers."
                .into(),
        );
    } else if judged != full && holds_one {
        out.notes.push(format!(
            "Get it gives it {} of its {} tokens: what fits beside what runs on the card now.",
            tokens(judged.unwrap_or(0)),
            tokens(full.unwrap_or(0)),
        ));
    } else if judged != full {
        out.notes.push(format!(
            "Get it gives it {} of its {} tokens: no single card holds the full context{}.",
            tokens(judged.unwrap_or(0)),
            tokens(full.unwrap_or(0)),
            match &out.min_cards.state {
                MinCardsState::SplitNotHere => ", and lmgw cannot split it here",
                MinCardsState::TooLarge => ", and all the cards together do not either",
                _ => "",
            }
        ));
    }
    out.experts_in_ram = experts;
    Some(out)
}

/// The fit over the cards of a file marked by its size alone (D8): Min.
/// cards by its bytes, Free VRAM on one card. `None` without a card.
pub fn by_size(bytes: Option<u64>, card: &CardNow) -> Option<CardsFit> {
    let cards = card.cards.as_deref();
    let of = cards.map_or(1, CardsNow::count);
    let mut out = CardsFit {
        of: u32::try_from(of).unwrap_or(u32::MAX),
        by_size: true,
        ..CardsFit::default()
    };
    let Some(bytes) = bytes else {
        let why = "Its size isn't known: the repo's file list gives none, or a part of it is \
                   missing.";
        out.min_cards = MinCards {
            label: "?".into(),
            sentence: why.into(),
            ..MinCards::default()
        };
        out.free = FreeVram {
            label: "?".into(),
            sentence: why.into(),
            ..FreeVram::default()
        };
        return Some(out);
    };
    let fp = crate::vram::Footprint::new(bytes, 0, None, None);
    let min = cards.map(|c| c.min_cards(bytes, None));
    let fit = FileFit::default();
    out.min_cards = min_cards(card, cards, min.as_ref(), &CanSplit::Yes, &fp, None, &fit);
    out.min_cards.sentence = format!(
        "By its size alone: {}",
        lower_first(&out.min_cards.sentence)
    );
    out.free = match &min {
        Some(Min::One(_)) | None => free_vram(card, cards, &fp, None, 1),
        Some(_) => FreeVram {
            label: "?".into(),
            sentence: "Which cards it would use is worked out once its header is read.".into(),
            ..FreeVram::default()
        },
    };
    Some(out)
}

/// Whether lmgw can split a file whose header says `cost` of what each
/// layer holds on `cards`.
fn can_split(chat: bool, cost: Option<&ByLayer>, cards: Option<&CardsNow>) -> CanSplit {
    if !chat {
        return CanSplit::No("this kind runs on one card".into());
    }
    let Some(cost) = cost else {
        return CanSplit::No(
            "its header does not say what each layer holds, so lmgw cannot split it".into(),
        );
    };
    let Some(cards) = cards else {
        return CanSplit::No(
            "a GPU memory budget is set and lmgw has no reading of the cards, so it cannot \
             split over them"
                .into(),
        );
    };
    if let Err(why) = &cards.hand_out {
        return CanSplit::No(format!("lmgw cannot split it here: {why}"));
    }
    match &cards.split {
        Ok(v) if cost.drafter && !v.devd => CanSplit::No(
            "lmgw cannot split it here: the chat class's llama-server has no -devd to keep its \
             draft model on the first card"
                .into(),
        ),
        Ok(_) => CanSplit::Yes,
        Err(why) => CanSplit::No(format!("lmgw cannot split it here: {why}")),
    }
}

/// "card 2", "cards 2 and 3", "cards 0, 1 and 3".
fn card_list(parts: &[CardPart]) -> String {
    let mut ix: Vec<u32> = parts.iter().map(|p| p.index).collect();
    ix.sort_unstable();
    ix.dedup();
    let s: Vec<String> = ix.iter().map(u32::to_string).collect();
    match s.as_slice() {
        [] => "no card".into(),
        [one] => format!("card {one}"),
        [rest @ .., last] => format!("cards {} and {last}", rest.join(", ")),
    }
}

/// What each card takes: "21.0 GiB on card 2, 5.6 GiB on card 3".
fn per_card(parts: &[CardPart]) -> String {
    parts
        .iter()
        .map(|p| format!("{} on card {}", fmt_bytes(p.bytes), p.index))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A badge's number of GiB: whole from 10 on, one decimal below.
fn gib(b: u64) -> String {
    // One decimal throughout, as every other figure on the card reads (a
    // bug found 2026-11-18: "needs 22 GiB, card 2 has 21" beside "21.8").
    let g = b as f64 / (1024.0 * 1024.0 * 1024.0);
    format!("{g:.1}")
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// "at 262 144 tokens", or nothing without a context.
fn at_ctx(ctx: Option<u64>) -> String {
    ctx.map_or(String::new(), |c| format!(" at {} tokens", tokens(c)))
}

/// Min. cards (module doc) from what the cards say at the full context.
fn min_cards(
    card: &CardNow,
    cards: Option<&CardsNow>,
    min: Option<&Min>,
    can: &CanSplit,
    fp: &crate::vram::Footprint,
    full: Option<u64>,
    fit: &FileFit,
) -> MinCards {
    let ctx = at_ctx(full);
    let Some(cards) = cards else {
        // A budget (or one pooled card): the budget's room.
        let needs = fp.total_bytes.saturating_add(card.card.headroom);
        if !card.card.budget {
            return MinCards {
                state: MinCardsState::OneCard,
                sign: if card.card.fits(fp) {
                    Sign::Ok
                } else {
                    Sign::No
                },
                cards: Some(1),
                sentence: "lmgw measures one graphics card here.".into(),
                ..MinCards::default()
            };
        }
        let fits = card.card.fits(fp);
        return MinCards {
            state: MinCardsState::Budget,
            sign: if fits { Sign::Ok } else { Sign::No },
            cards: None,
            label: "budget".into(),
            sentence: match fits {
                true => format!(
                    "It fits the GPU memory budget of {}{ctx}: it needs {}, what is kept free \
                     included.",
                    fmt_bytes(card.card.capacity),
                    fmt_bytes(needs)
                ),
                false => format!(
                    "It needs {}{ctx}, what is kept free included: more than the GPU memory \
                     budget of {}.",
                    fmt_bytes(needs),
                    fmt_bytes(card.card.capacity)
                ),
            },
            parts: Vec::new(),
        };
    };
    let of = cards.count();
    let one_card = of == 1;
    let shorter = matches!(fit.mark, FitMark::FitsShorter) && fit.ctx.is_some();
    let mut m = match min {
        Some(Min::One(p)) => MinCards {
            state: MinCardsState::Holds,
            sign: Sign::Ok,
            cards: Some(1),
            label: format!("1 of {of}"),
            sentence: format!(
                "Card {} ({}) holds it{ctx}: {} of its {}.",
                p.index,
                p.name,
                fmt_bytes(p.bytes),
                fmt_bytes(p.total_bytes)
            ),
            parts: vec![p.clone()],
        },
        Some(Min::Split(parts)) => {
            let n = parts.len();
            match can {
                CanSplit::Yes => MinCards {
                    state: MinCardsState::Holds,
                    sign: Sign::Ok,
                    cards: u32::try_from(n).ok(),
                    label: format!("{n} of {of}"),
                    sentence: format!(
                        "No single card holds it{ctx}; split over {} it takes {}.",
                        card_list(parts),
                        per_card(parts)
                    ),
                    parts: parts.clone(),
                },
                CanSplit::No(why) => MinCards {
                    state: MinCardsState::SplitNotHere,
                    sign: Sign::Warn,
                    cards: u32::try_from(n).ok(),
                    label: format!("{n} of {of}"),
                    sentence: format!("Fits only split{ctx}; {why}."),
                    parts: parts.clone(),
                },
            }
        }
        Some(Min::About(n)) => MinCards {
            state: match can {
                CanSplit::Yes => MinCardsState::Holds,
                CanSplit::No(_) => MinCardsState::SplitNotHere,
            },
            sign: Sign::Warn,
            cards: Some(*n),
            label: format!("{n} of {of}"),
            sentence: match can {
                CanSplit::Yes => format!(
                    "No single card holds it{ctx}; by its size about {n} cards do together."
                ),
                CanSplit::No(why) => format!("Fits only split{ctx}; {why}."),
            },
            parts: Vec::new(),
        },
        // What it needs and what is kept free beside it, against what the
        // cards hold (a bug found 2026-11-18: "it needs 3.1 GiB, and the
        // card hold 4.0 GiB" left out the 1.1 GiB kept beside it).
        Some(Min::TooLarge {
            needs,
            largest,
            all,
            kept,
        }) => MinCards {
            state: MinCardsState::TooLarge,
            sign: if shorter { Sign::Warn } else { Sign::No },
            cards: None,
            label: "too large".into(),
            sentence: format!(
                "Larger than {}{ctx}: it needs {} plus {} kept free beside it ({}), and {} {}{}.",
                if one_card {
                    "the graphics card".to_string()
                } else {
                    format!("all {of} cards together")
                },
                fmt_bytes(fp.total_bytes),
                fmt_bytes(match one_card {
                    true => needs.saturating_sub(fp.total_bytes),
                    false => *kept,
                }),
                match one_card {
                    true => "the engine's own cost and what the card keeps free for other programs",
                    false =>
                        "the engine's own cost and what each card keeps free for other programs",
                },
                if one_card {
                    "the card holds".to_string()
                } else {
                    format!("the {of} cards hold")
                },
                fmt_bytes(match one_card {
                    true => *largest,
                    false => *all,
                }),
                match (shorter, fit.ctx) {
                    (true, Some(c)) => format!("; one card holds {} tokens of it", tokens(c)),
                    _ => String::new(),
                }
            ),
            parts: Vec::new(),
        },
        None => MinCards::default(),
    };
    if one_card {
        // Hidden: a count of one card says nothing (the owner's ruling).
        m.state = match m.state {
            MinCardsState::Holds => MinCardsState::OneCard,
            s => s,
        };
        if m.state == MinCardsState::OneCard {
            m.label = String::new();
        }
    }
    m
}

/// Free VRAM (module doc) of footprint `fp` on `cards`, split by `cost`
/// where lmgw may split it; `k`: how many cards Min. cards counts.
fn free_vram(
    card: &CardNow,
    cards: Option<&CardsNow>,
    fp: &crate::vram::Footprint,
    cost: Option<Arc<ByLayer>>,
    k: usize,
) -> FreeVram {
    let Some(cards) = cards else {
        return pooled_free(card, fp);
    };
    let several = cards.count() > 1;
    match cards.free(fp.total_bytes, cost, k) {
        Free::Now(parts) => FreeVram {
            state: FreeState::Now,
            sign: Sign::Ok,
            label: "fits now".into(),
            sentence: match (parts.as_slice(), several) {
                ([p], false) => format!(
                    "The graphics card has room for it now: it takes {} there, {} is free.",
                    fmt_bytes(p.bytes),
                    fmt_bytes(p.free_bytes)
                ),
                ([p], true) => format!(
                    "Card {} ({}) has room for it now: it takes {} there, {} is free.",
                    p.index,
                    p.name,
                    fmt_bytes(p.bytes),
                    fmt_bytes(p.free_bytes)
                ),
                _ => format!(
                    "{} have room for it now, split: {}.",
                    upper_first(&card_list(&parts)),
                    per_card(&parts)
                ),
            },
            cards: parts,
            ..FreeVram::default()
        },
        Free::Unload(parts, names) => FreeVram {
            state: FreeState::AfterUnloading,
            sign: Sign::Warn,
            label: match names.as_slice() {
                [one] => format!("after unloading {one}"),
                _ => format!("after unloading {} models", names.len()),
            },
            sentence: format!(
                "It fits on {} once lmgw unloads its idle models there: {}.",
                where_(&parts, several),
                names.join(", ")
            ),
            cards: parts,
            unload: names,
            ..FreeVram::default()
        },
        Free::Wait(parts, names) => FreeVram {
            state: FreeState::AfterWaiting,
            sign: Sign::Warn,
            label: "after busy models finish".into(),
            sentence: format!(
                "It fits on {} once lmgw's busy models there finish and unload: {}.",
                where_(&parts, several),
                names.join(", ")
            ),
            cards: parts,
            unload: names,
            ..FreeVram::default()
        },
        Free::Short {
            needs,
            has,
            ours,
            cards: parts,
        } => {
            let label = match (parts.as_slice(), several) {
                (_, false) => format!("needs {} GiB, the card has {}", gib(needs), gib(has)),
                ([p], true) => format!(
                    "needs {} GiB, card {} has {}",
                    gib(needs),
                    p.index,
                    gib(has)
                ),
                _ => format!(
                    "needs {} GiB, {} have {}",
                    gib(needs),
                    card_list(&parts),
                    gib(has)
                ),
            };
            let there = match (parts.as_slice(), several) {
                (_, false) => "the graphics card has".to_string(),
                ([p], true) => format!("card {} ({}) has", p.index, p.name),
                _ => format!("{} have together", card_list(&parts)),
            };
            FreeVram {
                state: FreeState::Short,
                sign: Sign::No,
                sentence: format!(
                    "It needs {}; {there} {}{}.",
                    fmt_bytes(needs),
                    fmt_bytes(has),
                    if ours > 0 {
                        format!(
                            " with lmgw's models there unloaded ({} of it is theirs)",
                            fmt_bytes(ours)
                        )
                    } else {
                        " free".to_string()
                    }
                ),
                label,
                cards: parts,
                needs_bytes: Some(needs),
                free_bytes: Some(has),
                ..FreeVram::default()
            }
        }
    }
}

/// "card 2", or "the graphics card" on a computer of one.
fn where_(parts: &[CardPart], several: bool) -> String {
    match several {
        true => card_list(parts),
        false => "the graphics card".into(),
    }
}

fn upper_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Free VRAM where no card is read one by one (a budget): the pooled room
/// now, and the whole budget once lmgw's models unload.
fn pooled_free(card: &CardNow, fp: &crate::vram::Footprint) -> FreeVram {
    use lmgw_api_types::model_find::FitMeasure;
    // What it needs beside what admission keeps free by each measure, as
    // the one-card fit reads them (`CardNow::keep`): `whole` is the room
    // with lmgw's models unloaded already.
    let needs = fp.total_bytes.saturating_add(card.keep(FitMeasure::Beside));
    let needs_whole = fp
        .total_bytes
        .saturating_add(card.keep(FitMeasure::WholeCard));
    let now = card.room(FitMeasure::Beside);
    let whole = card.room(FitMeasure::WholeCard);
    let names: Vec<String> = card
        .residents
        .iter()
        .map(|r| format!("{}/{}", r.class, r.model))
        .collect();
    if needs <= now {
        FreeVram {
            state: FreeState::Now,
            sign: Sign::Ok,
            label: "fits now".into(),
            sentence: format!(
                "There is room for it now: it needs {}, {} is free.",
                fmt_bytes(needs),
                fmt_bytes(now)
            ),
            ..FreeVram::default()
        }
    } else if needs_whole <= whole && !names.is_empty() {
        FreeVram {
            state: FreeState::AfterUnloading,
            sign: Sign::Warn,
            label: match names.as_slice() {
                [one] => format!("after unloading {one}"),
                _ => format!("after unloading {} models", names.len()),
            },
            sentence: format!(
                "It fits once lmgw unloads its models: {}.",
                names.join(", ")
            ),
            unload: names,
            ..FreeVram::default()
        }
    } else {
        let (needs, has) = (needs_whole, whole);
        FreeVram {
            state: FreeState::Short,
            sign: Sign::No,
            label: format!("needs {} GiB, has {}", gib(needs), gib(has)),
            sentence: format!(
                "It needs {}, what is kept free included; {} is there for it with lmgw's models \
                 unloaded.",
                fmt_bytes(needs),
                fmt_bytes(has)
            ),
            needs_bytes: Some(needs),
            free_bytes: Some(has),
            ..FreeVram::default()
        }
    }
}

/// The third signal (module doc): `fp` is the footprint with every
/// layer's experts in RAM, `full` the footprint with them on the cards.
fn experts_in_ram(
    card: &CardNow,
    cards: Option<&CardsNow>,
    fp: &crate::vram::Footprint,
    full: &crate::vram::Footprint,
    cost: Option<Arc<ByLayer>>,
    can: &CanSplit,
    ctx: Option<u64>,
) -> ExpertsInRam {
    let ram = full.weights_bytes.saturating_sub(fp.weights_bytes);
    let free_k = |k| free_vram(card, cards, fp, cost.clone(), k);
    let (sign, n, parts, free) =
        match cards.map(|c| c.min_cards(fp.total_bytes, fp.by_layer.as_deref())) {
            Some(Min::One(p)) => (Sign::Ok, Some(1u32), vec![p], free_k(1)),
            Some(Min::Split(parts)) if *can == CanSplit::Yes => {
                let k = parts.len();
                (Sign::Ok, u32::try_from(k).ok(), parts, free_k(k))
            }
            Some(Min::Split(parts)) => (
                Sign::No,
                u32::try_from(parts.len()).ok(),
                parts,
                FreeVram::default(),
            ),
            Some(_) => (Sign::No, None, Vec::new(), FreeVram::default()),
            None => match card.card.fits(fp) {
                true => (Sign::Ok, Some(1), Vec::new(), free_k(1)),
                false => (Sign::No, None, Vec::new(), FreeVram::default()),
            },
        };
    let label = match n {
        Some(1) => "with experts in RAM: 1 card".to_string(),
        Some(k) => format!("with experts in RAM: {k} cards"),
        None => "with experts in RAM: too large".to_string(),
    };
    let sentence = match (sign, n) {
        (Sign::Ok, Some(_)) if !parts.is_empty() => format!(
            "With every layer's experts in the computer's memory ({}){}, {} holds the rest: {}. \
             It answers slower than with everything on the cards.",
            fmt_bytes(ram),
            at_ctx(ctx),
            card_list(&parts),
            per_card(&parts)
        ),
        (Sign::Ok, _) => format!(
            "With every layer's experts in the computer's memory ({}){}, it takes {} on the \
             graphics card. It answers slower than with everything on the card.",
            fmt_bytes(ram),
            at_ctx(ctx),
            fmt_bytes(fp.total_bytes)
        ),
        (_, Some(k)) => match can {
            CanSplit::No(why) => format!(
                "With its experts in RAM it still needs {k} cards{}; {why}.",
                at_ctx(ctx)
            ),
            CanSplit::Yes => format!("With its experts in RAM it still needs {k} cards."),
        },
        _ => format!(
            "Even with its experts in RAM it needs {}{}, more than the cards hold.",
            fmt_bytes(fp.total_bytes),
            at_ctx(ctx)
        ),
    };
    ExpertsInRam {
        sign,
        cards: n,
        label,
        sentence,
        ram_bytes: ram,
        parts,
        free,
    }
}

/// One of Get it's choices, with its words.
fn choice(
    layout: GetLayout,
    ctx: Option<u64>,
    min_parts: &[CardPart],
    ram: u64,
    free: FreeVram,
) -> GetChoice {
    // The cards it would use now where they have room, else the fewest
    // that hold it.
    let cards = match free.state {
        FreeState::Now | FreeState::AfterUnloading | FreeState::AfterWaiting
            if !free.cards.is_empty() =>
        {
            free.cards.clone()
        }
        _ => min_parts.to_vec(),
    };
    let ctx_words = ctx.map_or(String::new(), |c| format!(" at {} tokens", tokens(c)));
    let lead = match layout {
        GetLayout::Split => format!("Split over {}{ctx_words}", card_list(&cards)),
        GetLayout::OneCard => format!("On one card{ctx_words}"),
        GetLayout::ExpertsInRam => format!("With its experts in RAM{ctx_words}"),
    };
    let mut sentence = match (cards.is_empty(), layout) {
        (true, _) => lead,
        (false, GetLayout::ExpertsInRam) => format!(
            "{lead}: {}, and {} in RAM",
            per_card(&cards),
            fmt_bytes(ram)
        ),
        (false, _) => format!("{lead}: {}", per_card(&cards)),
    };
    sentence.push_str(match free.state {
        FreeState::Now => ". There is room now.",
        FreeState::AfterUnloading => ". lmgw unloads idle models first.",
        FreeState::AfterWaiting => ". It waits for busy models first.",
        FreeState::Short => ". There is not room now.",
        FreeState::Unknown => ".",
    });
    GetChoice {
        layout,
        ctx: ctx.unwrap_or(0),
        cards,
        ram_bytes: ram,
        free,
        sentence,
    }
}

#[cfg(test)]
mod tests;
