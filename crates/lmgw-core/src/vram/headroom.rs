//! VRAM admission's headroom (per-model containers §4, "Headroom"; approved by the
//! owner 2026-11-11).
//!
//! The headroom used to be one fixed setting (`vram.headroom_mb`, 1024 MiB)
//! doing two jobs: (a) room for other programs' GPU memory to grow after a
//! start (the desktop, a browser, a game), and (b) slack for lmgw's own
//! estimates between a model's start and its first measurement (the CUDA
//! context and allocator slack no header sizes; audio's on-disk estimate).
//! In `auto` mode (the default) each is sized from what it is for:
//!
//! **(a) Growth of outside use** ([`growth`]). What other programs hold now
//! is already out of "free" (the driver's figure), so only its growth
//! matters. A sampler reads the per-process attribution every
//! [`SAMPLE_INTERVAL`] — the card's used memory less what lmgw's containers
//! hold (§4.7) — and keeps, per hour, the lowest and highest reading and the
//! largest rise within it. The headroom is the largest rise over the last
//! [`WINDOW_DAYS`] days: the most any reading exceeded an earlier one
//! (hours apart included). Rises shorter than the interval are not seen.
//!
//! **(b) Slack while a model starts or is not measured** ([`VramScheduler::margin`]):
//! for a start being admitted and an entry still starting, and for an audio
//! row whose residency is not learned yet, the class's largest recorded gap
//! between its estimate and what its process held:
//! - llama.cpp (chat, aux): [`LLAMA_MARGIN`], 807 MiB, the largest gap in
//!   decision-models design Appendix C's "estimate against the process"
//!   table (the CUDA context, a projector's buffer, the CUDA pool's
//!   temporaries; Qwen3.8-Flash-Next at `--n-cpu-moe 43`, ubatch 1024,
//!   2026-11-09);
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
//! **Learned growth decides waiting** (the owner's ruling of 2026-11-11: it
//! decides only wait, evict or fall back, never whether a model can run at
//! all, and never shrinks a container for good). A start waits for, and
//! evicts, lmgw's own models until its footprint, the growth and its margin
//! are free. Once nothing of lmgw's is left on the card to wait for or
//! evict, a start that fits beside what other programs hold now with its
//! margin ([`VramScheduler::start_margin`]) goes ahead, with a warning that
//! names the growth and when it was seen ([`VramScheduler::growth_let_go`]).
//! The growth is never what a fallback answers for: §4.7's verdict asks
//! the card for the start and its margin only. Nor does it shrink a
//! llama-server row's fitted target (`partial::llama`), which is fitted to
//! what the card can give it now beside its margin.
//!
//! **Too large** ([`VramScheduler::card_headroom`], design decision of 2026-11-11): whether a model can run at all is
//! judged against the card less what other programs hold at rest (the
//! lowest reading of the window) and its margin, never against the learned
//! growth, which only decides whether there is room now — wait, evict or
//! fall back. No reading is taken while the hold or a benchmark keeps lmgw
//! off the card ([`growth::sample`]); the sentence names when the largest
//! growth was seen, and the owner can forget it (`headroom_forget`).
//!
//! **Until enough is learned** — fewer than [`HOURS_NEEDED`] hours of the
//! window with readings, or no per-process figures at all — auto uses
//! `headroom_mb`, the fixed value, for both jobs, with no margin on top, as
//! `fixed` mode always does. Settings says which ([`Self::headroom_now`]).
//!
//! **Pooled.** The readings are the pooled devices', as the ledger pools
//! them; per-device growth is not learned yet.

use std::time::Duration;

use lmgw_api_types::{HeadroomMargin, HeadroomMode, HeadroomNow};

use crate::config::Snapshot;
use crate::hf::fmt_bytes;
use crate::runtime::Class;

use super::plan::image::CONTEXT_STAND_IN;
use super::scheduler::MIB;
use super::{Footprint, VramScheduler};

pub(crate) mod growth;
pub use growth::{run, sample};

/// How often outside use is read.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(30);

/// The window the largest growth is taken over.
pub const WINDOW_DAYS: u64 = 7;

/// Hours of the window with readings before auto uses what it learned.
pub const HOURS_NEEDED: u64 = 24;

/// A llama-server's margin while it starts (module doc).
pub const LLAMA_MARGIN: u64 = 807 * MIB;

/// The lead of [`VramScheduler::growth_let_go`]'s warning.
pub const GROWTH_LET_GO: &str = "Learned growth not kept free";

/// An audio.cpp container's least margin until its residency is learned
/// (module doc): 0.78 GiB.
pub const AUDIO_MARGIN: u64 = 799 * MIB;

/// What admission keeps free for outside growth, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Headroom {
    /// What is kept free for other programs' growth.
    pub bytes: u64,
    /// The learned value is in use, so starting and unmeasured models get
    /// their margin on top ([`VramScheduler::margin`]).
    pub learned: bool,
}

impl VramScheduler {
    /// The headroom for outside growth now (module doc).
    pub fn headroom(&self, snap: &Snapshot) -> Headroom {
        let fixed = Headroom {
            bytes: snap.settings.vram.headroom_mb.saturating_mul(MIB),
            learned: false,
        };
        if snap.settings.vram.headroom_mode == HeadroomMode::Fixed {
            return fixed;
        }
        match self.growth.lock().unwrap().learned(growth::now_hour()) {
            Some(rise) => Headroom {
                bytes: rise,
                learned: true,
            },
            None => fixed,
        }
    }

    /// The margin for an entry of `class`/`model_id` with footprint `fp`
    /// that is starting or not measured yet (module doc): 0 unless the
    /// learned headroom is in use.
    pub fn margin(&self, snap: &Snapshot, class: Class, model_id: &str, fp: &Footprint) -> u64 {
        if !self.headroom(snap).learned {
            return 0;
        }
        margin_of(snap, class, model_id, fp)
    }

    /// What a start of `class`/`model_id` with footprint `fp` is admitted
    /// with beyond it: the headroom and its margin, or a fitted llama-server
    /// row's own `--fit-target` where that is more ([`own_target`]).
    pub fn start_headroom(
        &self,
        snap: &Snapshot,
        class: Class,
        model_id: &str,
        fp: &Footprint,
    ) -> u64 {
        let h = self.headroom(snap);
        let bytes = match h.learned {
            false => h.bytes,
            true => h.bytes.saturating_add(margin_of(snap, class, model_id, fp)),
        };
        own_target(fp).map_or(bytes, |t| t.max(bytes))
    }

    /// What a start of `class`/`model_id` with footprint `fp` needs beside
    /// it whatever other programs do next (module doc, "Learned growth
    /// decides waiting"): its class's margin with the learned headroom in
    /// use, else the fixed value — never the learned growth — or a fitted
    /// llama-server row's own `--fit-target` where that is more.
    pub fn start_margin(
        &self,
        snap: &Snapshot,
        class: Class,
        model_id: &str,
        fp: &Footprint,
    ) -> u64 {
        let h = self.headroom(snap);
        let bytes = match h.learned {
            false => h.bytes,
            true => margin_of(snap, class, model_id, fp),
        };
        own_target(fp).map_or(bytes, |t| t.max(bytes))
    }

    /// The warning of a start of `target` that goes ahead charged `bytes`
    /// with `free` free and only its `margin` kept beside it, where its
    /// `headroom` holds learned growth too (module doc, "Learned growth
    /// decides waiting"): the growth, when it was seen, and what it means.
    /// `None` where the growth was kept free, or none is in use.
    pub(crate) fn growth_let_go(
        &self,
        target: &super::Target,
        bytes: u64,
        (headroom, margin): (u64, u64),
        free: u64,
    ) -> Option<String> {
        if headroom <= margin || free >= bytes.saturating_add(headroom) {
            return None;
        }
        let growth = headroom - margin;
        let at = self
            .growth
            .lock()
            .unwrap()
            .largest(growth::now_hour())
            .map(|(_, hour)| format!(" on {}", hour_text(hour)))
            .unwrap_or_default();
        Some(format!(
            "{GROWTH_LET_GO}: other programs' GPU memory grew by up to {}{at} (auto headroom), \
             which VRAM admission keeps free beside a start while it can wait for or evict \
             lmgw's own models. None of lmgw's is left on the card, and {}/{} fits beside what \
             other programs hold now with its {} margin ({} for it, {} free), so it starts \
             without that room: should other programs grow that much again, the GPU can run out \
             of memory",
            fmt_bytes(growth),
            target.class.as_str(),
            target.model_id,
            fmt_bytes(margin),
            fmt_bytes(bytes),
            fmt_bytes(free),
        ))
    }

    /// What other programs hold on the card at rest, as learned (the
    /// lowest reading of the window): 0 where nothing was read yet.
    pub fn outside_at_rest(&self) -> u64 {
        self.growth
            .lock()
            .unwrap()
            .at_rest(growth::now_hour())
            .unwrap_or(0)
    }

    /// What other programs held on the card at the newest reading, else at
    /// rest, else 0: the preview's figure for a fitted llama-server row
    /// (`partial::llama`), which every decision fits again against the
    /// card as its own reading has it.
    pub fn outside_recent(&self) -> u64 {
        let g = self.growth.lock().unwrap();
        g.last
            .or_else(|| g.at_rest(growth::now_hour()))
            .unwrap_or(0)
    }

    /// What a start of `class`/`model_id` with footprint `fp` needs beside
    /// it on an otherwise empty card (module doc, "Too large"): the fixed
    /// value where that is the headroom; with the learned one in use, what
    /// other programs hold at rest and the class's margin — never the
    /// learned growth, which only decides whether there is room now.
    pub fn card_headroom(
        &self,
        snap: &Snapshot,
        class: Class,
        model_id: &str,
        fp: &Footprint,
    ) -> u64 {
        let h = self.headroom(snap);
        let bytes = match h.learned {
            false => h.bytes,
            true => self
                .outside_at_rest()
                .saturating_add(margin_of(snap, class, model_id, fp)),
        };
        own_target(fp).map_or(bytes, |t| t.max(bytes))
    }

    /// What Settings and `lmgw__settings` show (module doc).
    pub fn headroom_now(&self, snap: &Snapshot) -> HeadroomNow {
        let mode = snap.settings.vram.headroom_mode;
        let fixed = snap.settings.vram.headroom_mb.saturating_mul(MIB);
        let now = growth::now_hour();
        let (largest, hours_seen, at_rest) = {
            let g = self.growth.lock().unwrap();
            (g.largest(now), g.hours(now), g.at_rest(now))
        };
        let learned_bytes = largest.map(|(b, _)| b);
        let learned_at = largest.map(|(_, hour)| hour_text(hour));
        let h = self.headroom(snap);
        let measurable = self.probe().process_support().is_ok();
        let margins = vec![
            HeadroomMargin {
                class: "chat".into(),
                bytes: LLAMA_MARGIN,
                source: "the largest gap measured between a llama.cpp estimate and its process \
                         (the CUDA context and buffers no file sizes); kept until its first \
                         request has run, when the CUDA pool's temporaries are on the card"
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
        let days = WINDOW_DAYS;
        let every = SAMPLE_INTERVAL.as_secs();
        let watch = format!(
            "a reading every {every} s, the largest rise over the last {days} days, used once \
             {HOURS_NEEDED} hours have readings"
        );
        let margin_words = format!(
            "{} for a llama.cpp model (chat, embeddings; until its first request has run), {} \
             for an image model, as much as its estimate and at least {} for a voice model",
            fmt_bytes(LLAMA_MARGIN),
            fmt_bytes(CONTEXT_STAND_IN),
            fmt_bytes(AUDIO_MARGIN)
        );
        let sentence = match (mode, h.learned) {
            (HeadroomMode::Fixed, _) => format!(
                "Headroom: fixed, {}. It covers both other programs' growth and lmgw's own \
                 estimates while a model starts",
                fmt_bytes(fixed)
            ),
            (HeadroomMode::Auto, true) => format!(
                "Headroom: auto, {}: other programs grew by up to that much{} ({watch}). A \
                 model that is starting or not measured yet gets a margin on top: \
                 {margin_words}. Whether a model can run at all is judged without the growth: \
                 against the card less what other programs hold at rest ({}) and that margin. \
                 The growth only makes a start wait for or evict lmgw's own models: with none \
                 of them left, a start that fits beside what other programs hold now with its \
                 margin goes ahead, with a warning that names the growth",
                fmt_bytes(h.bytes),
                learned_at
                    .as_deref()
                    .map(|at| format!(", on {at}"))
                    .unwrap_or_default(),
                fmt_bytes(at_rest.unwrap_or(0)),
            ),
            (HeadroomMode::Auto, false) if !measurable => format!(
                "Headroom: auto, using the fixed {} for now: this machine gives no per-program \
                 GPU memory figures, so lmgw cannot tell other programs' use from its own",
                fmt_bytes(fixed)
            ),
            (HeadroomMode::Auto, false) => format!(
                "Headroom: auto, using the fixed {} until lmgw has watched the graphics card for \
                 {HOURS_NEEDED} hours ({hours_seen} so far; {watch}{}). Then a model that is \
                 starting or not measured yet gets a margin on top: {margin_words}",
                fmt_bytes(fixed),
                match (learned_bytes, &learned_at) {
                    (Some(b), Some(at)) => format!(
                        "; other programs grew by up to {} so far, on {at}",
                        fmt_bytes(b)
                    ),
                    _ => String::new(),
                }
            ),
        };
        HeadroomNow {
            mode,
            bytes: h.bytes,
            learned_in_use: h.learned,
            learned_bytes,
            learned_at,
            at_rest_bytes: at_rest,
            hours_seen,
            hours_needed: HOURS_NEEDED,
            window_days: days,
            sample_seconds: every,
            margins,
            sentence,
        }
    }

    /// Forget the learned growth (the owner's op `headroom_forget`):
    /// every reading, in memory and stored. Auto uses the fixed value
    /// until it has learned again.
    pub async fn forget_growth(&self, db: &sqlx::SqlitePool) -> Result<(), String> {
        self.growth.lock().unwrap().forget();
        crate::store::delete_vram_outside_hours(db)
            .await
            .map_err(|e| format!("the stored readings could not be deleted: {e}"))
    }

    /// Take `hours` in as the stored record, the way the sampler reads it
    /// back at boot: for tests that need a learned headroom.
    #[doc(hidden)]
    pub fn seed_outside_hours(&self, hours: Vec<crate::store::OutsideHour>) {
        self.growth.lock().unwrap().load(hours);
    }

    /// Record one reading of outside use, in bytes: the hour as it stands
    /// after it.
    pub(crate) fn note_outside(&self, outside: u64) -> crate::store::OutsideHour {
        self.growth
            .lock()
            .unwrap()
            .record(growth::now_hour(), outside)
    }
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

/// [`VramScheduler::margin`]'s figure, the learned headroom in use.
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
