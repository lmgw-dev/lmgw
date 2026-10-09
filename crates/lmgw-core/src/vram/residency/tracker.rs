//! What lmgw knows about each running audio container's residency, in memory
//! only: whether it has loaded its model and when it last answered, what it
//! held at rest, why its last reading failed, whether a sampler is watching
//! it — and, per row, the owner's resets and how settled its figure is.
//! Forgotten with the generation: a container started again is not loaded.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use super::SETTLE_AFTER;

/// Which audio container generations have loaded their model, as the
/// requests they answered tell it ([`crate::vram::LocalHold::note_inference`]),
/// and the rest of the per-generation and per-row state the readings keep.
#[derive(Debug, Default)]
pub(in crate::vram) struct Residency {
    inner: Mutex<Inner>,
    /// Readings finished, stored or not ([`crate::vram::VramScheduler::residency_readings`]).
    pub(super) readings: AtomicU64,
    /// Readings at rest finished, whatever they found.
    pub(super) at_rest_done: AtomicU64,
    /// Sampler stretches finished, whatever they saw.
    pub(super) stretches: AtomicU64,
    /// Sampler reads finished, whatever they found.
    pub(super) samples: AtomicU64,
    /// Held across "is this reading still current" and its store, and across
    /// the owner's reset — so a reading that began before a reset cannot
    /// write its figure back after it (WP7 review, low).
    pub(super) store: tokio::sync::Mutex<()>,
}

#[derive(Debug, Default)]
struct Inner {
    gens: HashMap<u64, Gen>,
    /// Model id → how many times the owner reset its figure. A reading or a
    /// sampler records the count it began under, and its figure is dropped
    /// when the count moved.
    epochs: HashMap<String, u64>,
    /// `(model id, key)` → sampled stretches in a row that did not raise the
    /// figure ([`SETTLE_AFTER`]).
    calm: HashMap<(String, String), u32>,
}

#[derive(Debug, Default, Clone)]
struct Gen {
    /// When it last answered an inference request — `Some` once it has
    /// loaded its model (or was taken as loaded at rest, [`AtRest::Loaded`]).
    last_answered: Option<Instant>,
    /// A reading after an answer is in flight ([`super::learn`]).
    measuring: bool,
    /// Why the last reading after an answer found no figure, until one does.
    failure: Option<String>,
    at_rest: AtRest,
    /// A sampler is watching its requests ([`super::sample`]).
    sampling: bool,
    /// The sampler's stretch saw an answer.
    stretch_answered: bool,
    /// How many times a figure of this generation raised the row's
    /// ([`Residency::raised`]); how many the last stretch to check had seen
    /// ([`Residency::raised_since`]); and the latter when its sampler began:
    /// the stretch raised it when the count moved past that.
    raises: u64,
    raises_checked: u64,
    raises_at_begin: u64,
    /// The largest figure the sampler saw, and the reset epoch it saw it in.
    sampled: Option<(u64, u64)>,
}

/// One reading of a ready container that had not answered anything and had
/// nothing in flight ([`super::ready`]) — what it holds at rest.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::vram) enum AtRest {
    /// Not taken: no per-process figures here, a request came first, or it
    /// has not been asked for yet.
    #[default]
    Unread,
    Reading,
    /// A lazy row's container holding no more than a bare context
    /// ([`super::looks_loaded`]): what is already on the card before its
    /// model loads, and again after audio.cpp unloads it for idling.
    Bare(u64),
    /// An eager row's container: its weights, already on the card.
    Eager(u64),
    /// A lazy row's container holding more than any bare context — its model
    /// is loaded (a container a previous lmgw left running, which answered
    /// requests then). Taken as loaded from the moment it was read.
    Loaded(u64),
    /// The reading failed, and why.
    Failed(String),
}

/// What one generation's state says, for the ledger and the notes.
#[derive(Debug, Default, Clone)]
pub(in crate::vram) struct GenView {
    pub(in crate::vram) last_answered: Option<Instant>,
    pub(in crate::vram) at_rest: AtRest,
    pub(in crate::vram) failure: Option<String>,
}

/// What [`Residency::note`] tells its caller to do.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::vram) struct Noted {
    /// It was not known as loaded before: the free figure just changed.
    pub(in crate::vram) newly_loaded: bool,
    /// No reading is in flight for it: start one.
    pub(in crate::vram) measure: bool,
}

/// How a sampler's stretch ended ([`Residency::end_sampling`]).
#[derive(Debug, PartialEq, Eq)]
pub(in crate::vram) struct Stretch {
    /// The largest figure it saw in the current reset epoch.
    pub(in crate::vram) max: Option<u64>,
    /// A request answered while it watched — only then do its figures count.
    pub(in crate::vram) answered: bool,
    /// The generation's raises it counts from: those the last stretch to
    /// check had seen when it began ([`Residency::raised_since`]).
    pub(in crate::vram) raises_at_begin: u64,
}

impl Residency {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A request on `generation` was answered now: it has loaded its model.
    pub(in crate::vram) fn note(&self, generation: u64) -> Noted {
        self.note_at(generation, Instant::now())
    }

    pub(in crate::vram) fn note_at(&self, generation: u64, now: Instant) -> Noted {
        let mut inner = self.lock();
        let g = inner.gens.entry(generation).or_default();
        let newly_loaded = g.last_answered.is_none();
        g.last_answered = Some(now);
        if g.sampling {
            g.stretch_answered = true;
        }
        let measure = !g.measuring;
        g.measuring = true;
        Noted {
            newly_loaded,
            measure,
        }
    }

    /// The reading for `generation` is over. `Ok(true)`: it read a figure;
    /// `Ok(false)`: it was skipped (no per-process figures here); `Err`: why
    /// it found none, kept for the notes until a reading finds one.
    pub(in crate::vram) fn measured(&self, generation: u64, outcome: Result<bool, String>) {
        let read = !matches!(outcome, Ok(false));
        if let Some(g) = self.lock().gens.get_mut(&generation) {
            g.measuring = false;
            match outcome {
                Ok(true) => g.failure = None,
                Ok(false) => {}
                Err(why) => g.failure = Some(why),
            }
        }
        if read {
            self.readings.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether `why` is news for `generation`: its last reading did not fail
    /// with the same reason. A container whose readings can never find a
    /// figure (one on the CPU backend holds nothing on the card) fails every
    /// one of them alike, after every request it answers.
    pub(in crate::vram) fn failure_is_new(&self, generation: u64, why: &str) -> bool {
        self.lock()
            .gens
            .get(&generation)
            .is_none_or(|g| g.failure.as_deref() != Some(why))
    }

    /// Whether `generation` has answered a request (or was read loaded).
    #[cfg(test)]
    pub(in crate::vram) fn loaded(&self, generation: u64) -> bool {
        self.lock()
            .gens
            .get(&generation)
            .is_some_and(|g| g.last_answered.is_some())
    }

    /// Every generation among `live`, forgetting the ones that left.
    pub(in crate::vram) fn view(&self, live: &[u64]) -> HashMap<u64, GenView> {
        let mut inner = self.lock();
        inner.gens.retain(|g, _| live.contains(g));
        inner
            .gens
            .iter()
            .map(|(id, g)| {
                (
                    *id,
                    GenView {
                        last_answered: g.last_answered,
                        at_rest: g.at_rest.clone(),
                        failure: g.failure.clone(),
                    },
                )
            })
            .collect()
    }

    /// One generation's state, forgetting nothing.
    pub(in crate::vram) fn get(&self, generation: u64) -> Option<GenView> {
        self.lock().gens.get(&generation).map(|g| GenView {
            last_answered: g.last_answered,
            at_rest: g.at_rest.clone(),
            failure: g.failure.clone(),
        })
    }

    /// Claim the at-rest reading of `generation`: only once, and only for a
    /// container that has not answered anything.
    pub(in crate::vram) fn begin_at_rest(&self, generation: u64) -> bool {
        let mut inner = self.lock();
        let g = inner.gens.entry(generation).or_default();
        if g.last_answered.is_some() || g.at_rest != AtRest::Unread {
            return false;
        }
        g.at_rest = AtRest::Reading;
        true
    }

    /// The at-rest reading's result. [`AtRest::Loaded`] marks the generation
    /// loaded as of `now`. A reading a request overtook (it answered
    /// meanwhile) is dropped: it is not a figure of the container at rest.
    pub(in crate::vram) fn at_rest(&self, generation: u64, result: AtRest, now: Instant) {
        self.at_rest_done.fetch_add(1, Ordering::Relaxed);
        let mut inner = self.lock();
        let Some(g) = inner.gens.get_mut(&generation) else {
            return;
        };
        if g.last_answered.is_some() {
            g.at_rest = AtRest::Unread;
            return;
        }
        if matches!(result, AtRest::Loaded(_)) {
            g.last_answered = Some(now);
        }
        g.at_rest = result;
    }

    /// The owner's reset count for `model_id`.
    pub(in crate::vram) fn epoch(&self, model_id: &str) -> u64 {
        self.lock().epochs.get(model_id).copied().unwrap_or(0)
    }

    /// The owner reset `model_id`'s figure: every reading and sampler that
    /// began before now is dropped, and its figure settles again from zero.
    pub(in crate::vram) fn reset(&self, model_id: &str) {
        let mut inner = self.lock();
        *inner.epochs.entry(model_id.to_string()).or_insert(0) += 1;
        inner.calm.retain(|(m, _), _| m != model_id);
    }

    /// Start watching `generation`'s requests — unless a sampler already is.
    pub(in crate::vram) fn begin_sampling(&self, generation: u64) -> bool {
        let mut inner = self.lock();
        let g = inner.gens.entry(generation).or_default();
        if g.sampling {
            return false;
        }
        g.sampling = true;
        g.stretch_answered = false;
        // Not the raises so far: one that landed after the last stretch
        // checked was that stretch's to see and it missed it, so this one
        // counts it — settling one stretch later rather than never seeing it.
        g.raises_at_begin = g.raises_checked;
        true
    }

    /// One sample: keep it when it is the largest of `epoch`.
    pub(in crate::vram) fn sample(&self, generation: u64, epoch: u64, bytes: u64) {
        if let Some(g) = self.lock().gens.get_mut(&generation) {
            g.sampled = match g.sampled {
                Some((e, b)) if e == epoch => Some((e, b.max(bytes))),
                _ => Some((epoch, bytes)),
            };
        }
    }

    /// The largest sample of `generation` taken in `epoch`.
    pub(in crate::vram) fn sampled(&self, generation: u64, epoch: u64) -> Option<u64> {
        self.lock()
            .gens
            .get(&generation)
            .and_then(|g| g.sampled)
            .filter(|(e, _)| *e == epoch)
            .map(|(_, b)| b)
    }

    /// A figure of `generation` raised the row's: counted under the store
    /// lock ([`super::learn`]'s `store_resident`): a stretch that checks
    /// after it sees it then, and one that checked before it leaves it to
    /// the next stretch ([`Residency::raised_since`]). A raise by the
    /// reading after an answer — which folds the stretch's samples in — used
    /// to be credited to the stretch only while it still watched; one that
    /// landed after the stretch's end was lost, and the stretch counted
    /// towards settling though its samples had raised the figure.
    pub(in crate::vram) fn raised(&self, generation: u64) {
        if let Some(g) = self.lock().gens.get_mut(&generation) {
            g.raises += 1;
        }
    }

    /// Whether a figure of `generation` raised the row's since `at`
    /// ([`Stretch::raises_at_begin`]), asked once by a stretch after its own
    /// store: what it saw is checked, and the next stretch counts from
    /// there ([`Residency::begin_sampling`]). A raise by the reading after
    /// an answer can still land after this check — when the sampler's own
    /// store kept the figure — and is then the next stretch's.
    pub(in crate::vram) fn raised_since(&self, generation: u64, at: u64) -> bool {
        let mut inner = self.lock();
        let Some(g) = inner.gens.get_mut(&generation) else {
            return false;
        };
        g.raises_checked = g.raises;
        g.raises > at
    }

    /// The sampler of `generation` stopped.
    pub(in crate::vram) fn end_sampling(&self, generation: u64, epoch: u64) -> Stretch {
        let mut inner = self.lock();
        let Some(g) = inner.gens.get_mut(&generation) else {
            return Stretch {
                max: None,
                answered: false,
                raises_at_begin: 0,
            };
        };
        g.sampling = false;
        Stretch {
            max: g.sampled.filter(|(e, _)| *e == epoch).map(|(_, b)| b),
            answered: std::mem::take(&mut g.stretch_answered),
            raises_at_begin: g.raises_at_begin,
        }
    }

    /// A sampled stretch with an answer is over: count it towards settling
    /// `key`'s figure, or start the count again when it raised the figure.
    pub(in crate::vram) fn stretch_done(&self, model_id: &str, key: &str, raised: bool) {
        let mut inner = self.lock();
        let calm = inner
            .calm
            .entry((model_id.to_string(), key.to_string()))
            .or_insert(0);
        *calm = if raised { 0 } else { calm.saturating_add(1) };
    }

    /// Sampled stretches in a row that did not raise `key`'s figure.
    pub(in crate::vram) fn calm(&self, model_id: &str, key: &str) -> u32 {
        self.lock()
            .calm
            .get(&(model_id.to_string(), key.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// Whether `key`'s figure has settled: [`SETTLE_AFTER`] sampled stretches
    /// in a row did not raise it, so its requests are not sampled any more.
    pub(in crate::vram) fn settled(&self, model_id: &str, key: &str) -> bool {
        self.calm(model_id, key) >= SETTLE_AFTER
    }
}
