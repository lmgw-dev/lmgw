//! VRAM admission's headroom (per-model containers §4, "Headroom"; approved by the
//! owner 2026-11-11; per card and as a floor with learned room on top since
//! the device ledger's WP3 and WP13, 2026-11-17).
//!
//! What a start keeps free beside it on a card is two things: room for other
//! programs (the desktop, a browser, `ollama`, a document converter), and
//! slack for lmgw's own estimate while the model starts.
//!
//! **Room for other programs**, per card ([`VramScheduler::cards_headroom`]).
//! What they hold now is already out of "free" (the driver's figure), so the
//! room is what they may take on top. A sampler reads the per-process
//! attribution every [`SAMPLE_INTERVAL`] — the card's used memory less what
//! lmgw's containers hold (§4.7) — and keeps, per card and hour, the lowest
//! and the highest reading ([`growth`]). In `auto` mode (the default) a card
//! keeps:
//! - **the floor**: `headroom_mb` (1 GiB by default), always, but no more than
//!   the most other programs held on the card in the last [`WINDOW_DAYS`]
//!   days: a card nobody else uses keeps nothing, and a display server's few
//!   MiB do not cost 1 GiB. A card without readings keeps all of it;
//! - **the learned room**, on top: the most other programs held there in the
//!   window less what they hold now (the newest reading), so they can come
//!   back to their most. It applies from the first reading.
//!
//! In `fixed` mode a card keeps `headroom_mb` as written. The owner can
//! declare a card's amount instead (`vram.keep_free`, Settings → GPU):
//! "always" N MiB, never given up, or "nothing" — the card is lmgw's.
//!
//! **The learned room and the margin are given up last** (WP13; the margin
//! too by the owner's decision of 2026-11-17). Placing a model, a card's
//! room is counted net of its whole headroom and the start's margin first —
//! on one card now, then after evicting idle models of lmgw's (on any
//! card), then over several cards — and net of the floor alone only where
//! nothing else fits; then they decide only wait, evict or fall back, never
//! whether a model can run at all (the owner's ruling of 2026-11-11: a
//! learned figure never refuses a model that fits beside the floor). Once
//! nothing of lmgw's is left on a card to wait for or evict, a start that
//! fits there beside the floor and its margin goes ahead without the room;
//! one that fits beside the floor alone, and nothing smaller of it beside
//! the margin (a row that adapts is fitted smaller first), goes ahead
//! without its margin too; each with a warning that names what it took
//! ([`VramScheduler::growth_let_go`]). The margin goes after the room: the
//! room is a guess of what other programs may take again, the margin what
//! the start itself is likely to take on top of its estimate (WP6's split
//! took 239 MiB of its 327), so a card whose room can go is taken before
//! one where the margin would have to. The cost, accepted by the owner: an
//! estimate that runs low then comes out of the floor. The floor and a
//! declared amount are never given up: a model that would need them waits,
//! goes on another card or falls back.
//!
//! **Too large** ([`VramScheduler::card_headroom`]): whether a model can run
//! at all is judged against the card less what other programs hold at rest
//! (the lowest reading of the window) and the floor; never against the
//! learned room nor the margin, so a voice model whose files fit a card
//! starts there and has its residency learned. In `fixed` mode, and on a
//! card with a declared amount, against the card less that amount. No
//! reading is taken while the hold or a benchmark keeps lmgw off the card
//! ([`growth::sample`]); the owner can forget a card's readings
//! (`headroom_forget`).
//!
//! **Slack while a model starts or is not measured** ([`VramScheduler::margin`]),
//! on top of the room for other programs in every mode (WP13: WP6's split
//! took 239 MiB more than planned, out of the room meant for others), and
//! given up last as above: for a start being admitted and an entry still
//! starting, and for an audio row whose residency is not learned yet, the
//! class's largest recorded gap between its estimate and what its process
//! held:
//! - llama.cpp (chat, aux): [`LLAMA_MARGIN`], 327 MiB: the largest gap in
//!   decision-models design Appendix C's "estimate against the process"
//!   table, 807 MiB (Qwen3.8-Flash-Next at `--n-cpu-moe 43`, ubatch 1024,
//!   2026-11-09), less the engine's own cost on a card, which is charged
//!   with the model since the device ledger's WP5 (`vram::place::engine`):
//!   less the smallest WP0 measured (480 MiB, an RTX 6000 Ada), so what is
//!   left is the most the start's slack (a projector's buffer, the CUDA
//!   pool's temporaries) can be;
//! - sd.cpp (image): [`crate::vram::plan::image::CONTEXT_STAND_IN`], 1.0 GiB,
//!   the most an sd-server context was measured holding at rest; none for a
//!   partial start, whose grant sets its context aside already;
//! - audio.cpp: as much as its estimate (measured residency is about twice
//!   the on-disk size, realtime design §9.4), at least [`AUDIO_MARGIN`],
//!   0.78 GiB, the largest gap measured (pocket-tts, 0.90 GiB held against
//!   0.12 GiB on disk).
//!
//! Once the driver shows an entry (an sd-server that is ready, a
//! llama-server once a request that runs a prompt or encodes has run on it —
//! its first prompt brings the CUDA pool's temporaries; a token count runs
//! none and leaves the margin) or its residency is learned (audio), its
//! margin is dropped: the measurement is the figure. A llama-server adopted
//! from an earlier lmgw has served an unknown number of requests: where the
//! driver gives per-process figures, what it holds is read, and that reading
//! is its measurement — no margin; without such figures nothing of it was
//! ever measured, and it keeps the margin until a request has run on it.
//! llama.cpp rows learn no footprint of their own yet, so the reading is the
//! only measurement an adopted one can have.
//!
//! **Sums.** Where a decision reads one figure for the machine (the pooled
//! capacity of a declared budget without telemetry, the climb, the
//! previews) it reads the cards' sums; the margin rides once, being a
//! model's, not a card's. The cards are the ones the last device read
//! listed ([`growth::Record::counted`]); on a machine of one card every
//! figure is that card's.
//!
//! The engine's own cost per card (the CUDA context on each card it
//! touches) is the model's charge (device-ledger design D5, WP5,
//! `vram::place::engine`), in neither figure.

use std::time::Duration;

use lmgw_api_types::{
    HeadroomCard, HeadroomMargin, HeadroomMode, HeadroomNow, KeepFree, KeepFreeMode,
    KeepFreeSuggestion,
};

use crate::config::Snapshot;
use crate::hf::fmt_bytes;
use crate::runtime::Class;

use super::nvml::DeviceId;
use super::plan::image::CONTEXT_STAND_IN;
use super::scheduler::MIB;
use super::{Footprint, VramScheduler};

pub(crate) mod growth;
pub use growth::{run, sample};

/// How often outside use is read: every second (the owner, 2026-11-19).
/// Other programs' use comes in bursts of seconds: a 30 h log of a
/// four-card test server at one reading a second showed an
/// embedding server rising up to 5.2 GiB for 1 to 10 s at a time, and
/// nothing else moving. Read every 30 s, that log gave a learned room
/// between 1.3 and 5.2 GiB depending on where the readings fell (3.3 GiB
/// typically); read every 2 or 3 s it gave 5.2 GiB however they fell. A
/// reading is the driver's per-process and device figures and lmgw's
/// ledger, no process started; the store is written only when a reading
/// changes the card's hour ([`growth::Growth::record`]).
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// The window the most other programs held is taken over.
pub const WINDOW_DAYS: u64 = 7;

/// A llama-server's margin while it starts (module doc): the start's slack
/// alone, now that the engine's own cost per card is charged with the model
/// (device-ledger design §4.2, §9.1).
pub const LLAMA_MARGIN: u64 = 327 * MIB;

/// The lead of [`VramScheduler::growth_let_go`]'s warning where the learned
/// room was given up (with the margin or not).
pub const GROWTH_LET_GO: &str = "Learned growth not kept free";

/// The lead of [`VramScheduler::growth_let_go`]'s warning where only the
/// start's margin was given up: the card keeps no learned room.
pub const MARGIN_LET_GO: &str = "Start margin not kept free";

/// An audio.cpp container's least margin until its residency is learned
/// (module doc): 0.78 GiB.
pub const AUDIO_MARGIN: u64 = 799 * MIB;

/// What admission keeps free for other programs, every card's together
/// ([`VramScheduler::cards_headroom`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Headroom {
    /// The floors and the learned room of every card.
    pub bytes: u64,
}

/// One card's part of the headroom (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardHeadroom {
    /// The card; `None` while no card is known (no device read, no reading
    /// yet), where `headroom_mb` is kept once.
    pub id: Option<DeviceId>,
    pub index: u32,
    /// The driver's name; empty for a card no device read listed.
    pub name: String,
    /// What the owner declared for the card (`vram.keep_free`).
    pub keep: KeepFree,
    /// Always kept free here: the floor, the fixed value or the declared
    /// amount.
    pub floor: u64,
    /// The learned room on top, given up last; 0 outside `auto`.
    pub room: u64,
    /// `floor` and `room`: what a start keeps free here while it can make
    /// room another way.
    pub bytes: u64,
    /// What a model needs beside it here with nothing else of lmgw's on
    /// the card (§5.8), its margin aside: what other programs hold here at
    /// rest and the floor in `auto`, else the floor alone.
    pub structural: u64,
    /// The most other programs held here in the window, and the first hour
    /// they did; `None` without readings.
    pub most: Option<(u64, i64)>,
    /// Other programs' use here at rest; `None` without readings.
    pub at_rest: Option<u64>,
    /// The newest reading here, else at rest; `None` without readings.
    pub recent: Option<u64>,
    /// Hours of the window with readings here.
    pub hours: u64,
    /// The separate times in the window other programs held more than
    /// `recent` and the floor together (the suggestion's count).
    pub times: u64,
}

impl CardHeadroom {
    /// The card by name and index, as every surface names a card
    /// ("RTX PRO 6000 Blackwell (card 2)"); never its id.
    pub fn label(&self) -> String {
        label_of(&self.name, self.index)
    }
}

fn label_of(name: &str, index: u32) -> String {
    match name.is_empty() {
        false => format!("{name} (card {index})"),
        true => format!("card {index}"),
    }
}

/// A card's floor, learned room and what it needs beside a model at rest
/// (module doc), from what the owner declared for it, the mode, the fixed
/// value, and what other programs did there: their most, what they hold at
/// rest and now.
fn parts(
    keep: KeepFree,
    auto: bool,
    fixed: u64,
    (most, at_rest, recent): (Option<u64>, Option<u64>, Option<u64>),
) -> (u64, u64, u64) {
    match keep.mode {
        KeepFreeMode::Nothing => (0, 0, 0),
        KeepFreeMode::Always => {
            let b = keep.mb.saturating_mul(MIB);
            (b, 0, b)
        }
        KeepFreeMode::Automatic if !auto => (fixed, 0, fixed),
        KeepFreeMode::Automatic => {
            let floor = most.map_or(fixed, |m| fixed.min(m));
            let room = match (most, recent) {
                (Some(m), Some(r)) => m.saturating_sub(r),
                _ => 0,
            };
            (floor, room, at_rest.unwrap_or(0).saturating_add(floor))
        }
    }
}

/// What forgetting dropped of one card ([`VramScheduler::forget_growth`]),
/// for the audit line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forgotten {
    pub id: DeviceId,
    /// The card by name and index.
    pub label: String,
    /// The most other programs held there in the readings; `None` without
    /// readings.
    pub most: Option<u64>,
    /// The hours of readings it rested on.
    pub hours: u64,
    /// The learned room they kept free there.
    pub room: u64,
}

impl Forgotten {
    /// What was dropped, in words.
    pub fn words(&self) -> String {
        match (self.most, self.room) {
            (None, _) => "no readings".to_string(),
            (Some(m), 0) => format!(
                "readings of other programs up to {}, from {} hours",
                fmt_bytes(m),
                self.hours
            ),
            (Some(m), room) => format!(
                "readings of other programs up to {}, from {} hours, with {} of learned room \
                 kept free",
                fmt_bytes(m),
                self.hours,
                fmt_bytes(room)
            ),
        }
    }
}

/// The audit line's target of a forget of card `card` (every card's with
/// `None`) that dropped `dropped` (device-ledger design ruling 10): the
/// card by name and index with its id, and the figure that was dropped.
pub fn audit_target(
    card: Option<&DeviceId>,
    dropped: &[Forgotten],
) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    match (card, dropped) {
        (Some(id), [one]) => {
            m.insert("card".into(), one.label.clone().into());
            m.insert("card_id".into(), id.as_str().into());
            m.insert("dropped".into(), one.words().into());
        }
        _ => {
            m.insert("card".into(), "every card".into());
            let words: Vec<String> = dropped
                .iter()
                .map(|f| format!("{} ({}): {}", f.label, f.id, f.words()))
                .collect();
            m.insert(
                "dropped".into(),
                match words.is_empty() {
                    true => "no readings".to_string(),
                    false => words.join("; "),
                }
                .into(),
            );
        }
    }
    m
}

/// The sums of the cards' parts that a decision reading one figure for the
/// machine reads (module doc, "Sums").
pub(in crate::vram) struct Sums {
    /// Every card's floor and learned room together.
    pub bytes: u64,
    /// Every card's floor.
    pub floor: u64,
    /// Every card's learned room.
    pub room: u64,
    /// What every card needs beside a model at rest.
    pub structural: u64,
}

pub(in crate::vram) fn sums(cards: &[CardHeadroom]) -> Sums {
    let add = |f: &dyn Fn(&CardHeadroom) -> u64| cards.iter().map(f).fold(0, u64::saturating_add);
    Sums {
        bytes: add(&|c| c.bytes),
        floor: add(&|c| c.floor),
        room: add(&|c| c.room),
        structural: add(&|c| c.structural),
    }
}

impl VramScheduler {
    /// Each card's headroom now (module doc), in the driver's order.
    pub fn cards_headroom(&self, snap: &Snapshot) -> Vec<CardHeadroom> {
        let fixed = snap.settings.vram.headroom_mb.saturating_mul(MIB);
        let auto = snap.settings.vram.headroom_mode == HeadroomMode::Auto;
        let now = growth::now_hour();
        let record = self.growth.lock().unwrap();
        record
            .counted()
            .into_iter()
            .map(|c| {
                let g = c.id.as_ref().and_then(|id| record.card(id));
                let keep =
                    c.id.as_ref()
                        .and_then(|id| snap.settings.vram.keep_free.get(id))
                        .copied()
                        .unwrap_or_default();
                let most = g.and_then(|g| g.most(now));
                let at_rest = g.and_then(|g| g.at_rest(now));
                let recent = g.and_then(|g| g.recent(now));
                let (floor, room, structural) =
                    parts(keep, auto, fixed, (most.map(|m| m.0), at_rest, recent));
                CardHeadroom {
                    keep,
                    floor,
                    room,
                    bytes: floor.saturating_add(room),
                    structural,
                    most,
                    at_rest,
                    recent,
                    hours: g.map_or(0, |g| g.hours(now)),
                    times: g.map_or(0, |g| {
                        g.times_above(now, recent.unwrap_or(0).saturating_add(floor))
                    }),
                    id: c.id,
                    index: c.index,
                    name: c.name,
                }
            })
            .collect()
    }

    /// What is kept free for other programs now (module doc): every card's
    /// together.
    pub fn headroom(&self, snap: &Snapshot) -> Headroom {
        Headroom {
            bytes: sums(&self.cards_headroom(snap)).bytes,
        }
    }

    /// The margin for an entry of `class`/`model_id` with footprint `fp`
    /// that is starting or not measured yet (module doc): on top of the
    /// room for other programs in every mode.
    pub fn margin(&self, snap: &Snapshot, class: Class, model_id: &str, fp: &Footprint) -> u64 {
        margin_of(snap, class, model_id, fp)
    }

    /// What a start of `class`/`model_id` with footprint `fp` keeps free
    /// beside it while it can make room another way: the cards' floors and
    /// learned room and its margin, or a fitted llama-server row's own
    /// `--fit-target` where that is more ([`own_target`]).
    pub fn start_headroom(
        &self,
        snap: &Snapshot,
        class: Class,
        model_id: &str,
        fp: &Footprint,
    ) -> u64 {
        let bytes = sums(&self.cards_headroom(snap))
            .bytes
            .saturating_add(margin_of(snap, class, model_id, fp));
        own_target(fp).map_or(bytes, |t| t.max(bytes))
    }

    /// What a start of `class`/`model_id` with footprint `fp` keeps free
    /// beside it once the learned room is given up (module doc, "given up
    /// last"): the cards' floors (or declared amounts) and its class's
    /// margin, or a fitted llama-server row's own `--fit-target` where that
    /// is more.
    pub fn start_margin(
        &self,
        snap: &Snapshot,
        class: Class,
        model_id: &str,
        fp: &Footprint,
    ) -> u64 {
        let bytes = sums(&self.cards_headroom(snap))
            .floor
            .saturating_add(margin_of(snap, class, model_id, fp));
        own_target(fp).map_or(bytes, |t| t.max(bytes))
    }

    /// What a start with footprint `fp` keeps free beside it once the
    /// learned room and its margin are both given up (module doc, "given up
    /// last"): the cards' floors (or declared amounts) alone, or a fitted
    /// llama-server row's own `--fit-target` where that is more, which is
    /// the row's own and never given up.
    pub fn start_floor(&self, snap: &Snapshot, fp: &Footprint) -> u64 {
        let bytes = sums(&self.cards_headroom(snap)).floor;
        own_target(fp).map_or(bytes, |t| t.max(bytes))
    }

    /// The warning of a start of `target` that goes ahead charged `bytes`
    /// with `free` free, where it keeps `headroom` (the floor, the learned
    /// room and its margin) while it can make room another way, `margin`
    /// (the floor and its margin) once the room is given up and `floor` once
    /// the margin is too (module doc): what it went ahead without — the
    /// room, where other programs held that much, the margin — and what
    /// that means. `None` where it kept them.
    pub(crate) fn growth_let_go(
        &self,
        target: &super::Target,
        bytes: u64,
        (headroom, margin, floor): (u64, u64, u64),
        free: u64,
    ) -> Option<String> {
        let room_gone = headroom > margin && free < bytes.saturating_add(headroom);
        let margin_gone = margin > floor && free < bytes.saturating_add(margin);
        if !room_gone && !margin_gone {
            return None;
        }
        let model = format!("{}/{}", target.class.as_str(), target.model_id);
        let kept = match floor {
            0 => "with nothing kept free for other programs here".to_string(),
            f => format!("with the {} always kept free", fmt_bytes(f)),
        };
        let gap = margin.saturating_sub(floor);
        if !room_gone {
            return Some(format!(
                "{MARGIN_LET_GO}: VRAM admission keeps a margin of {} beside {model} while it \
                 starts and is not measured yet, for what its estimate may miss. None of \
                 lmgw's is left to wait for or evict, and it fits beside what other programs \
                 hold now {kept}, not with that margin ({} for it, {} free), so it starts \
                 without it: should it take more than its estimate, {}",
                fmt_bytes(gap),
                fmt_bytes(bytes),
                fmt_bytes(free),
                match floor {
                    0 => "the GPU can run out of memory".to_string(),
                    f => format!(
                        "that comes out of the {} kept free for other programs",
                        fmt_bytes(f)
                    ),
                }
            ));
        }
        let room = headroom - margin;
        let now = growth::now_hour();
        // The card where other programs held the most beyond what they hold
        // now, and when.
        let (most, several) = {
            let record = self.growth.lock().unwrap();
            let counted = record.counted();
            let most = counted
                .iter()
                .filter_map(|c| {
                    let g = record.card(c.id.as_ref()?)?;
                    let (m, hour) = g.most(now)?;
                    let above = m.saturating_sub(g.recent(now)?);
                    (above > 0).then(|| (above, m, hour, label_of(&c.name, c.index)))
                })
                .fold(None::<(u64, u64, i64, String)>, |best, x| match best {
                    Some(b) if b.0 >= x.0 => Some(b),
                    _ => Some(x),
                });
            (most, counted.len() > 1)
        };
        let at = match (most, several) {
            (Some((_, m, hour, _)), false) => {
                format!(" (they held {} on {})", fmt_bytes(m), hour_text(hour))
            }
            (Some((_, m, hour, label)), true) => format!(
                " (on {label} they held {} on {})",
                fmt_bytes(m),
                hour_text(hour)
            ),
            (None, _) => String::new(),
        };
        Some(match margin_gone {
            false => format!(
                "{GROWTH_LET_GO}: other programs have used up to {} more GPU memory than they \
                 hold now{at}, which VRAM admission keeps free beside a start while it can make \
                 room another way (auto headroom). None of lmgw's is left to wait for or evict, \
                 and {model} fits beside what other programs hold now {kept}{} ({} for it, {} \
                 free), so it starts without that room: should other programs use that much \
                 again, the GPU can run out of memory",
                fmt_bytes(room),
                match gap {
                    0 => String::new(),
                    g => format!(" and its {} margin", fmt_bytes(g)),
                },
                fmt_bytes(bytes),
                fmt_bytes(free),
            ),
            true => format!(
                "{GROWTH_LET_GO}: other programs have used up to {} more GPU memory than they \
                 hold now{at}, which VRAM admission keeps free beside a start while it can make \
                 room another way (auto headroom), and on top a margin of {} for what the \
                 start's estimate may miss. None of lmgw's is left to wait for or evict, and \
                 {model} fits beside what other programs hold now {kept}, not with that room \
                 or that margin ({} for it, {} free), so it starts without either: should \
                 other programs use that much again, or it take more than its estimate, the \
                 GPU can run out of memory",
                fmt_bytes(room),
                fmt_bytes(gap),
                fmt_bytes(bytes),
                fmt_bytes(free),
            ),
        })
    }

    /// What other programs hold on the cards at rest, as learned (the
    /// lowest reading of the window, each card's together): 0 where
    /// nothing was read yet.
    pub fn outside_at_rest(&self) -> u64 {
        self.over_cards(|g, now| g.at_rest(now))
    }

    /// What other programs held on the cards at the newest reading, else at
    /// rest, else 0, each card's together: the preview's figure for a
    /// fitted llama-server row (`partial::llama`), which every decision
    /// fits again against the card as its own reading has it.
    pub fn outside_recent(&self) -> u64 {
        self.over_cards(|g, now| g.recent(now))
    }

    /// `f` of each counted card's readings, summed; a card without
    /// readings counts 0.
    fn over_cards(&self, f: impl Fn(&growth::Growth, i64) -> Option<u64>) -> u64 {
        let now = growth::now_hour();
        let record = self.growth.lock().unwrap();
        record
            .counted()
            .iter()
            .filter_map(|c| record.card(c.id.as_ref()?))
            .filter_map(|g| f(g, now))
            .fold(0, u64::saturating_add)
    }

    /// What a start with footprint `fp` needs beside it on otherwise empty
    /// cards (module doc, "Too large"): on each card what other programs
    /// hold there at rest and its floor (in `auto`), or its fixed value or
    /// declared amount — never the learned room nor the start's margin,
    /// which only decide whether there is room now and are given up last —
    /// or a fitted llama-server row's own `--fit-target` where that is
    /// more.
    pub fn card_headroom(&self, snap: &Snapshot, fp: &Footprint) -> u64 {
        let bytes = sums(&self.cards_headroom(snap)).structural;
        own_target(fp).map_or(bytes, |t| t.max(bytes))
    }

    /// What Settings and `lmgw__settings` show (module doc).
    pub fn headroom_now(&self, snap: &Snapshot) -> HeadroomNow {
        let mode = snap.settings.vram.headroom_mode;
        let fixed = snap.settings.vram.headroom_mb.saturating_mul(MIB);
        let cards = self.cards_headroom(snap);
        let s = sums(&cards);
        let at_rest = cards
            .iter()
            .filter_map(|c| c.at_rest)
            .fold(None, |a: Option<u64>, b| {
                Some(a.unwrap_or(0).saturating_add(b))
            });
        let hours_seen = cards.iter().map(|c| c.hours).min().unwrap_or(0);
        let measurable = self.probe().process_support().is_ok();
        let margins = vec![
            HeadroomMargin {
                class: "chat".into(),
                bytes: LLAMA_MARGIN,
                source: "the largest gap measured between a llama.cpp estimate and its process, \
                         less the engine's own cost on a card, which is charged with the model: \
                         buffers no file sizes; kept until its first request has run, when the \
                         CUDA pool's temporaries are on the card"
                    .into(),
            },
            HeadroomMargin {
                class: "aux".into(),
                bytes: LLAMA_MARGIN,
                source: "as for chat: the same engine".into(),
            },
            HeadroomMargin {
                class: "image".into(),
                bytes: CONTEXT_STAND_IN,
                source: "the most an image engine's context was measured holding".into(),
            },
            HeadroomMargin {
                class: "audio".into(),
                bytes: AUDIO_MARGIN,
                source: "as much as its estimate, at least this: a voice model was measured \
                         holding about twice its files, up to this much more"
                    .into(),
            },
        ];
        let sentence = machine_sentence(
            (mode, measurable),
            (fixed, s.bytes, s.room),
            &cards,
            &margin_words(),
        );
        let engine_of = |id: &DeviceId| self.engine_cost(id);
        let engine_sentence = engine_sentence(&cards, &engine_of);
        let card_lines = cards
            .iter()
            .filter_map(|c| {
                c.id.as_ref().map(|id| HeadroomCard {
                    engine_bytes: engine_of(id).bytes,
                    engine_learned_at: engine_of(id).learned_at.map(second_text),
                    id: id.clone(),
                    index: c.index,
                    name: c.name.clone(),
                    keep: c.keep,
                    bytes: c.bytes,
                    floor_bytes: c.floor,
                    room_bytes: c.room,
                    most_bytes: c.most.map(|(b, _)| b),
                    most_at: c.most.map(|(_, hour)| hour_text(hour)),
                    now_bytes: c.recent,
                    at_rest_bytes: c.at_rest,
                    hours_seen: c.hours,
                    sentence: card_sentence(c, mode, fixed, measurable),
                    suggest: suggestion(c, mode),
                })
            })
            .collect();
        HeadroomNow {
            mode,
            bytes: s.bytes,
            floor_bytes: s.floor,
            room_bytes: s.room,
            at_rest_bytes: at_rest,
            hours_seen,
            window_days: WINDOW_DAYS,
            sample_seconds: SAMPLE_INTERVAL.as_secs(),
            margins,
            sentence,
            cards: card_lines,
            engine_sentence,
        }
    }

    /// Forget the readings of other programs' use (the owner's op
    /// `headroom_forget`, device-ledger design ruling 10): card `card`'s,
    /// or every card's with `None`, in memory and stored. That card, or
    /// every card, keeps the whole `headroom_mb` and no learned room until
    /// its next reading. The answer is what was dropped, per card, for the
    /// audit line; `Err` for a card that is neither listed nor has
    /// readings.
    pub async fn forget_growth(
        &self,
        db: &sqlx::SqlitePool,
        snap: &Snapshot,
        card: Option<&DeviceId>,
    ) -> Result<Vec<Forgotten>, String> {
        let cards = self.cards_headroom(snap);
        let forgotten = {
            let mut record = self.growth.lock().unwrap();
            let now = growth::now_hour();
            let of = |id: &DeviceId| {
                let c = cards.iter().find(|c| c.id.as_ref() == Some(id));
                let g = record.card(id);
                Forgotten {
                    id: id.clone(),
                    label: c.map_or_else(
                        || "a card no device read lists now".to_string(),
                        CardHeadroom::label,
                    ),
                    most: g.and_then(|g| g.most(now)).map(|(b, _)| b),
                    hours: g.map_or(0, |g| g.hours(now)),
                    room: c.map_or(0, |c| c.room),
                }
            };
            let forgotten: Vec<Forgotten> = match card {
                Some(id) => {
                    let known = cards.iter().any(|c| c.id.as_ref() == Some(id))
                        || record.card(id).is_some();
                    if !known {
                        return Err(format!(
                            "no graphics card {id} is listed here or has readings; {}",
                            cards_named(&cards)
                        ));
                    }
                    vec![of(id)]
                }
                None => {
                    let mut ids: Vec<DeviceId> =
                        cards.iter().filter_map(|c| c.id.clone()).collect();
                    for id in record.absent() {
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                    }
                    ids.iter().map(of).collect()
                }
            };
            record.forget(card);
            forgotten
        };
        for f in &forgotten {
            tracing::info!(
                "VRAM headroom: the readings of {} ({}) were forgotten: {}",
                f.label,
                f.id,
                f.words()
            );
        }
        crate::store::delete_vram_outside_hours(db, card)
            .await
            .map_err(|e| format!("the stored readings could not be deleted: {e}"))?;
        Ok(forgotten)
    }

    /// Whether `ids` are cards an amount can be declared for
    /// (`vram.keep_free`): each one listed by the last device read or with
    /// readings. `Err` names the first that is not, and the cards there are.
    pub fn known_cards(&self, ids: &[&DeviceId]) -> Result<(), String> {
        let record = self.growth.lock().unwrap();
        let counted = record.counted();
        let absent = record.absent();
        for id in ids {
            let known = counted.iter().any(|c| c.id.as_ref() == Some(*id)) || absent.contains(id);
            if !known {
                let names: Vec<String> = counted
                    .iter()
                    .filter_map(|c| {
                        let id = c.id.as_ref()?;
                        Some(format!("{} ({id})", label_of(&c.name, c.index)))
                    })
                    .collect();
                return Err(format!(
                    "vram.keep_free: no graphics card {id} is listed here or has readings; {}",
                    match names.is_empty() {
                        true => "no card is known yet".to_string(),
                        false => format!("the cards are {}", names.join(", ")),
                    }
                ));
            }
        }
        Ok(())
    }

    /// `patch` merged into `keep_free` (the setting `vram.keep_free`, WP13
    /// C): only the cards given change, and `automatic` takes a card's
    /// entry out. `always` needs at least 1 MiB; a card set to `always` or
    /// `nothing` must be one this machine lists or has readings of
    /// ([`Self::known_cards`]). Nothing changes where one entry is refused.
    pub fn merge_keep_free(
        &self,
        keep_free: &mut std::collections::BTreeMap<DeviceId, KeepFree>,
        patch: std::collections::BTreeMap<DeviceId, KeepFree>,
    ) -> Result<(), String> {
        for (id, k) in &patch {
            if k.mode == KeepFreeMode::Always && k.mb == 0 {
                return Err(format!(
                    "vram.keep_free: {id} is set to always keep 0 MiB free; give at least 1 MiB, \
                     or mode \"nothing\""
                ));
            }
        }
        let declared: Vec<&DeviceId> = patch
            .iter()
            .filter(|(id, k)| k.mode != KeepFreeMode::Automatic && keep_free.get(*id) != Some(*k))
            .map(|(id, _)| id)
            .collect();
        self.known_cards(&declared)?;
        for (id, k) in patch {
            match k.mode {
                KeepFreeMode::Automatic => {
                    keep_free.remove(&id);
                }
                KeepFreeMode::Always => {
                    keep_free.insert(id, k);
                }
                KeepFreeMode::Nothing => {
                    keep_free.insert(
                        id,
                        KeepFree {
                            mode: k.mode,
                            mb: 0,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// Take `hours` of card `device` in as the stored record, the way the
    /// sampler reads it back at boot: for tests that need readings.
    #[doc(hidden)]
    pub fn seed_outside_hours(&self, device: &DeviceId, hours: Vec<crate::store::OutsideHour>) {
        self.growth
            .lock()
            .unwrap()
            .load(hours.into_iter().map(|h| (device.clone(), h)).collect());
    }

    /// Record one reading of outside use on card `device`, in bytes: the
    /// card's hour as it stands after it, when the reading changed it.
    pub(crate) fn note_outside(
        &self,
        device: &DeviceId,
        outside: u64,
    ) -> Option<crate::store::OutsideHour> {
        self.growth
            .lock()
            .unwrap()
            .record(device, growth::now_hour(), outside)
    }
}

/// The known cards by name and index with their ids, for a refusal.
fn cards_named(cards: &[CardHeadroom]) -> String {
    let names: Vec<String> = cards
        .iter()
        .filter_map(|c| Some(format!("{} ({})", c.label(), c.id.as_ref()?)))
        .collect();
    match names.is_empty() {
        true => "no card is known yet".to_string(),
        false => format!("the cards are {}", names.join(", ")),
    }
}

/// The margins in words, for the sentence.
fn margin_words() -> String {
    format!(
        "{} for a llama.cpp model (chat, embeddings; until its first request has run), {} for \
         an image model, as much as its estimate and at least {} for a voice model",
        fmt_bytes(LLAMA_MARGIN),
        fmt_bytes(CONTEXT_STAND_IN),
        fmt_bytes(AUDIO_MARGIN)
    )
}

/// [`VramScheduler::headroom_now`]'s sentence for the machine: how the room
/// for other programs is set and what it is now, the cards with an amount
/// of their own, and the margin on top.
fn machine_sentence(
    (mode, measurable): (HeadroomMode, bool),
    (fixed, bytes, room): (u64, u64, u64),
    cards: &[CardHeadroom],
    margin_words: &str,
) -> String {
    let n = cards.len();
    let several = n > 1;
    let declared: Vec<String> = cards
        .iter()
        .filter(|c| c.keep.mode != KeepFreeMode::Automatic)
        .map(CardHeadroom::label)
        .collect();
    let margin = format!(
        "A model that is starting or not measured yet gets a margin on top, in every mode, given \
         up last with the learned room, where nothing else fits beside the floor: \
         {margin_words}"
    );
    let own = match declared.len() {
        0 => String::new(),
        _ if !several => " The card keeps the amount set for it.".to_string(),
        _ => format!(
            " {} {} the amount set for {}.",
            super::place::in_words(&declared),
            if declared.len() == 1 { "keeps" } else { "keep" },
            if declared.len() == 1 { "it" } else { "them" }
        ),
    };
    let lead = match (mode, measurable) {
        (HeadroomMode::Fixed, _) if several => format!(
            "Headroom: fixed, {} kept free for other programs on each of the {n} graphics \
             cards.",
            fmt_bytes(fixed)
        ),
        (HeadroomMode::Fixed, _) => format!(
            "Headroom: fixed, {} kept free for other programs.",
            fmt_bytes(fixed)
        ),
        (HeadroomMode::Auto, false) => format!(
            "Headroom: auto, {} kept free for other programs on {} for now: this machine gives \
             no per-program GPU memory figures, so lmgw cannot tell their use from its own.",
            fmt_bytes(fixed),
            if several { "each card" } else { "the card" }
        ),
        (HeadroomMode::Auto, true) => format!(
            "Headroom: auto, from what other programs did on {}: a floor of {} is always kept \
             free for them, but no more than the most they held there in the last \
             {WINDOW_DAYS} days, and on top the learned room, the most they held less what \
             they hold now (a reading every {} s). lmgw gives the learned room up last: it \
             uses another card, evicts its own idle models or waits for them first, and \
             never refuses a model that fits beside the floor.",
            if several { "each card" } else { "the card" },
            fmt_bytes(fixed),
            SAMPLE_INTERVAL.as_secs()
        ),
    };
    let now = match (several, room) {
        (true, 0) => format!(
            " {} kept free on the {n} cards together now.",
            fmt_bytes(bytes)
        ),
        (true, r) => format!(
            " {} kept free on the {n} cards together now, {} of it learned room.",
            fmt_bytes(bytes),
            fmt_bytes(r)
        ),
        (false, _) if mode == HeadroomMode::Fixed && declared.is_empty() => String::new(),
        (false, 0) => format!(" {} kept free now.", fmt_bytes(bytes)),
        (false, r) => format!(
            " {} kept free now, {} of it learned room.",
            fmt_bytes(bytes),
            fmt_bytes(r)
        ),
    };
    format!("{lead}{own}{now} {margin}")
}

/// What a llama.cpp model is charged on each card for the engine itself
/// (device-ledger design §4.2), for Settings: each card by name and index
/// with its figure — the largest of its last readings (owner, 2026-11-18)
/// and when the newest was taken — or the stated default.
fn engine_sentence(
    cards: &[CardHeadroom],
    cost: &dyn Fn(&DeviceId) -> crate::vram::place::engine::EngineCost,
) -> String {
    let parts: Vec<String> = cards
        .iter()
        .filter_map(|c| {
            let id = c.id.as_ref()?;
            let e = cost(id);
            let how = match e.learned_at {
                Some(at) => format!("learned, newest reading {}", second_text(at)),
                None if crate::vram::place::engine::is_cuda(id) => {
                    "the CUDA default until a model has started on this card alone".to_string()
                }
                None => "the default for a card that is not NVIDIA's, until a model has \
                         started on it alone"
                    .to_string(),
            };
            Some(format!("{}: {} ({how})", c.label(), fmt_bytes(e.bytes)))
        })
        .collect();
    match parts.is_empty() {
        true => String::new(),
        false => format!(
            "What a llama.cpp model is charged on each card for the engine itself (its CUDA \
             context and what else it keeps on every card it uses), learned per card after a \
             start on one card as the largest of its last {} readings: {}",
            crate::vram::place::engine::KEPT,
            parts.join("; ")
        ),
    }
}

/// Unix seconds `at` in local time, for sentences.
fn second_text(at: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(at, 0) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => {
            t.format("%Y-%m-%d %H:%M").to_string()
        }
        chrono::LocalResult::None => format!("second {at}"),
    }
}

/// One card's sentence, beside its name in Settings (device-ledger design
/// WP13): what is always kept free, what other programs used here and use
/// now, and the learned room kept on top.
/// Learned room below this is left out of a card's sentence ([`card_sentence`]).
const SMALL_ROOM: u64 = 64 * MIB;

fn card_sentence(c: &CardHeadroom, mode: HeadroomMode, fixed: u64, measurable: bool) -> String {
    let seen = match (c.most, c.recent) {
        (Some((m, hour)), Some(now)) => Some(format!(
            "They have used up to {} here ({}) and use {} now",
            fmt_bytes(m),
            hour_text(hour),
            fmt_bytes(now)
        )),
        _ => None,
    };
    let seen_tail = seen.as_ref().map(|s| format!(" {s}.")).unwrap_or_default();
    match c.keep.mode {
        KeepFreeMode::Nothing => {
            format!(
                "Nothing is kept free for other programs: this card is lmgw's, as set.{seen_tail}"
            )
        }
        KeepFreeMode::Always => format!(
            "{} is always kept free for other programs, as set for this card: a model that \
             would need it waits, goes on another card or falls back.{seen_tail}",
            fmt_bytes(c.floor)
        ),
        KeepFreeMode::Automatic if mode == HeadroomMode::Fixed => format!(
            "{} is always kept free for other programs (fixed headroom).{seen_tail}",
            fmt_bytes(fixed)
        ),
        KeepFreeMode::Automatic if c.most.is_none() && !measurable => format!(
            "{} is always kept free for other programs: this machine gives no per-program GPU \
             memory figures, so nothing is learned here.",
            fmt_bytes(fixed)
        ),
        KeepFreeMode::Automatic if c.most.is_none() => format!(
            "{} is always kept free for other programs; no readings here yet.",
            fmt_bytes(fixed)
        ),
        KeepFreeMode::Automatic => {
            let floor = match c.floor {
                0 => format!(
                    "Nothing is always kept free: no other program has used this card in the \
                     last {WINDOW_DAYS} days."
                ),
                f if f < fixed => format!(
                    "{} is always kept free for other programs, the most they held here (less \
                     than the {} floor).",
                    fmt_bytes(f),
                    fmt_bytes(fixed)
                ),
                f => format!("{} is always kept free for other programs.", fmt_bytes(f)),
            };
            let room = match (seen, c.room) {
                (None, _) => String::new(),
                (Some(seen), 0) if c.floor == 0 => format!(" {seen}."),
                (Some(seen), 0) => format!(" {seen}: nothing more is kept free."),
                // A few MiB of learned room is the readings' jitter, not
                // a figure to read (owner, 2026-11-18): left out of the
                // sentence, still kept.
                (Some(seen), r) if r < SMALL_ROOM => format!(" {seen}."),
                (Some(seen), r) => format!(
                    " {seen}: lmgw keeps the other {} free while it can make room another way.",
                    fmt_bytes(r)
                ),
            };
            format!("{floor}{room}")
        }
    }
}

/// The amount Settings suggests keeping free on card `c` always (WP13 C):
/// on an `automatic` card in `auto` mode where other programs used more
/// than they hold now and its floor together at least once in the window,
/// the floor and the most they used beyond what they hold now, in whole
/// MiB — "always" replaces the floor, so the amount carries it (owner,
/// 2026-11-17). Per card, once: never per model.
fn suggestion(c: &CardHeadroom, mode: HeadroomMode) -> Option<KeepFreeSuggestion> {
    if mode != HeadroomMode::Auto
        || c.keep.mode != KeepFreeMode::Automatic
        || c.room == 0
        || c.times == 0
    {
        return None;
    }
    let mb = c.floor.saturating_add(c.room).div_ceil(MIB);
    let times = match c.times {
        1 => "once".to_string(),
        2 => "twice".to_string(),
        n => format!("{n} times"),
    };
    let floor = match c.floor {
        0 => String::new(),
        f => format!("the {} floor and ", fmt_bytes(f)),
    };
    Some(KeepFreeSuggestion {
        mb,
        times: c.times,
        sentence: format!(
            "Other programs used up to {} more than now, {times} in {WINDOW_DAYS} days: always \
             keep {} free ({floor}that {})?",
            fmt_bytes(c.room),
            fmt_bytes(mb.saturating_mul(MIB)),
            fmt_bytes(c.room)
        ),
    })
}

/// A llama-server row's own `--fit-target`, where admission fits it in
/// `--fit`'s place: what it asks to keep free beside it, which `--fit off`
/// leaves llama.cpp nothing to keep with, so admission keeps it.
fn own_target(fp: &Footprint) -> Option<u64> {
    fp.llama.as_ref().and_then(|l| l.sizer.fit_target)
}

/// Hour `hour` (Unix time / 3600) in local time, for sentences.
fn hour_text(hour: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(hour.saturating_mul(3600), 0) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => {
            t.format("%Y-%m-%d %H:00").to_string()
        }
        chrono::LocalResult::None => format!("hour {hour}"),
    }
}

/// [`VramScheduler::margin`]'s figure, in every headroom mode (WP13).
pub(crate) fn margin_of(snap: &Snapshot, class: Class, model_id: &str, fp: &Footprint) -> u64 {
    let audio_learned = (class == Class::Audio)
        .then(|| {
            snap.audio_models
                .iter()
                .find(|m| m.model_id == model_id)
                .map(|m| super::residency::learned(m, &snap.settings.audio).is_some())
        })
        .flatten();
    class_margin(class, fp, audio_learned)
}

/// The margin of `class` for footprint `fp` (module doc). `audio_learned`:
/// whether an audio row's residency is learned, `None` where `fp` is
/// already what it is charged (a learned residency has no margin, and the
/// caller does not know).
fn class_margin(class: Class, fp: &Footprint, audio_learned: Option<bool>) -> u64 {
    match class {
        Class::Chat | Class::Aux if fp.total_bytes == 0 => 0,
        Class::Chat | Class::Aux => LLAMA_MARGIN,
        // A pipeline whose context is measured, or set aside already.
        Class::Image
            if fp
                .partial
                .as_ref()
                .is_some_and(|p| p.context_bytes.is_some()) =>
        {
            0
        }
        Class::Image => CONTEXT_STAND_IN,
        Class::Audio if audio_learned == Some(true) => 0,
        Class::Audio if fp.total_bytes == 0 => 0,
        Class::Audio => fp.total_bytes.max(AUDIO_MARGIN),
    }
}

#[cfg(test)]
mod tests;
