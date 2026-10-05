//! The learned transient peak of an image pipeline (image-generation design
//! §9 and §12.4).
//!
//! **The gap.** Every other class costs what it costs: llama-server uploads
//! weights and a KV cache once and holds them, so the driver's `used` figure
//! *is* the answer and the ledger ([`super::VramScheduler`]) can admit against
//! it. An sd-server pipeline does not behave that way. Measured on the
//! 4090: Z-Image-Turbo Q4_K is **7.1 GiB resident when idle** and needs
//! **13.7 GiB** while it renders one 1024² image, because the compute buffers
//! (≈ 6.6 GiB at that size, resolution-dependent) are allocated per job and
//! freed after it. With the pipeline idle the ledger therefore reads ~16 GiB
//! free on a 24 GiB card, admits a 15 GiB chat model into it, and the next
//! generation OOMs — having broken nothing that any surface could have warned
//! about.
//!
//! **What is learned, and from what.** Per image row, `peak_extra_bytes`: the
//! largest `device used − baseline` seen across samples taken while that row
//! had `in_flight > 0`, where the baseline is the device's `used` from the
//! last sample taken while the row was `ready` and **idle** — or, for a window
//! that had no such sample before it, the first one after it ([`Window`]). The
//! *extra* is the figure, not the peak itself, and deliberately: the idle
//! residency is already inside the driver's `used`, so it is already
//! subtracted from the free memory admission measures. What admission is blind
//! to is exactly the part that is not there yet.
//!
//! **Monotonic.** A 1024² job teaches the number; a later 256² job does not
//! lower it. The charge has to cover the largest job this pipeline has been
//! asked for, not the last one.
//!
//! **Device-wide, because the probe is.** NVML answers `used/free/total` per
//! *device*; there are no per-process figures here and inventing a
//! PID → container mapping is out of scope (§13). Two consequences, both
//! deliberate:
//!
//! * If two image rows are in flight at once, the whole delta is attributed to
//!   **each** of them. Over-charging is the safe direction — the alternative
//!   is splitting a number nobody measured.
//! * A window during which anything else started or stopped is **abandoned**,
//!   not learned from ([`Window::shape`]). A chat model loading beside a
//!   generation would otherwise write its weights into this row's peak and
//!   keep them there forever, with the only evidence a figure on a status
//!   page that looks plausible. A window whose residency changed teaches
//!   nothing, which costs one generation and no correctness. That includes
//!   an audio model loading inside a container that was `ready` all along
//!   (audio.cpp loads on its first request, realtime design §9.4): the
//!   window's shape carries which audio containers hold their model
//!   ([`Shape::audio`]), and one that is loading spoils every window it
//!   overlaps.
//!
//! **A window that saw no rise teaches nothing either.** A 0 delta is
//! indistinguishable from a job that finished between two samples (the 256²
//! shape renders in 0.23 s), so recording it would turn a sampling miss into a
//! durable claim that this pipeline needs nothing. The row stays unlearned and
//! says so.
//!
//! **Cadence**, three rates, each doing one job:
//! [`IN_FLIGHT_INTERVAL`] while a generation is running,
//! [`RESIDENT_INTERVAL`] while a pipeline is up and idle (the baseline half),
//! and [`IDLE_INTERVAL`] — a registry map read, no driver call — while no
//! image container exists at all. A baseline read during the container's own
//! *start*, when the weights are still going onto the card, would make the
//! first generation look like it allocated the whole pipeline, so `starting`
//! is never a baseline.
//!
//! The figure this produces is a **sampled maximum**: what the driver was seen
//! holding, never an interpolation between two readings. A transient shorter
//! than one interval is missed, the row keeps whatever it had, and the next
//! generation gets another go — which is the same reason the value is a
//! running maximum rather than the last measurement. (Being woken by the
//! registry when a guard is taken would replace only the idle half — the
//! in-flight half still has to poll — and the idle half is where the baseline
//! comes from.)
//!
//! **No telemetry, nothing learned.** Where the probe answers `Err` (no NVML,
//! no amdgpu, CI) this task reads the registry and goes back to sleep. The
//! row's `peak_extra_bytes` stays NULL and `GET /api/vram` says, on the
//! resident itself, that the peak is not learned and what to do about it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::hf::fmt_bytes;
use crate::runtime::registry::{RuntimeState, RuntimeView};
use crate::runtime::Class;
use crate::state::SharedState;

/// How often the driver is read while a generation is actually running.
///
/// Not a bound on anything and nothing is skipped because of it — the loop
/// simply looks this often, and what it does not see it does not claim. The
/// spike's own 4 Hz (§12) is enough for the shape that motivated all of this,
/// a 1024² render whose 13.7 GiB plateau lasts the whole multi-second job. It
/// is **not** enough for the short ones: measured on this box while building
/// §9, a 512² render's rise (1.6-1.8 GiB) came and went inside a single 250 ms
/// gap on one run and was caught on the next. 20 Hz costs one
/// `nvmlDeviceGetMemoryInfo` per 50 ms *for the seconds a job is in flight*
/// and nothing at all otherwise, which is the cheapest honest answer available
/// to a sampler.
pub const IN_FLIGHT_INTERVAL: Duration = Duration::from_millis(50);

/// How often the driver is read while an image pipeline is up and idle.
///
/// This is the baseline half, and a baseline does not move: a resident
/// pipeline holds the same bytes until it is asked to draw. What the rate has
/// to buy is a *fresh* reading whenever a window opens — so it is the spike's
/// 4 Hz, and a pipeline that sits resident overnight costs four driver reads a
/// second rather than twenty.
pub const RESIDENT_INTERVAL: Duration = Duration::from_millis(250);

/// How often the registry is looked at when no image container is up at all.
///
/// A map read and nothing else — no driver call, no syscall — so what this
/// number buys is how late the sampler can be to a container that has just
/// appeared. The idle reaper's 15 s was the obvious value and is **measured
/// wrong for this job**: on this box the whole first generation (a 3.2 s cold
/// start and a 1 s render) happened inside one such sleep, so the sampler woke
/// to an idle pipeline and learned nothing about a job it had never seen. One
/// second is shorter than any sd-server start — the measured eager load is
/// 2.2 s (§12.3) — so the sampler is watching before the pipeline can be asked
/// to draw, which is the only property this interval has to have.
pub const IDLE_INTERVAL: Duration = Duration::from_secs(1);

/// What lmgw is holding on to between samples. One entry per image model that
/// currently has a container.
#[derive(Debug, Default)]
struct Row {
    /// Device `used` at the last sample where this row was `ready` and idle —
    /// the pipeline's idle residency, plus whatever else is on the card.
    baseline: Option<u64>,
    /// What was on the card at that sample (WP7 review M2). A window opens
    /// against the baseline of an *earlier* tick, so an audio model that
    /// loaded between that tick and this one is in this tick's `used` but not
    /// in the baseline — and would be learned as this pipeline's peak. The
    /// window is clean only when this equals the shape it opens with, and no
    /// audio load was in progress at the baseline either.
    baseline_shape: Option<Shape>,
    /// The in-flight window currently open, if any.
    window: Option<Window>,
}

#[derive(Debug)]
struct Window {
    /// The idle reading this window is measured against, captured when it
    /// opened — `None` when there was none, which is the first request after a
    /// start: `acquire` hands its guard out the moment the container answers
    /// its readiness probe, so the pipeline can go from `starting` to busy
    /// without ever being sampled idle. Such a window is measured against the
    /// first idle reading *after* it instead, which is the same quantity read
    /// from the other side: the compute buffers are freed when the job ends
    /// (measured, §12.4 — "idle returns to 7.1 GiB"), so what is left is the
    /// residency. Without that fallback a pipeline with a short `idle_seconds`
    /// — started per request, reaped after it — could never learn anything at
    /// all, because every one of its windows would be a first one.
    baseline: Option<u64>,
    /// The largest device `used` seen inside it.
    max_used: u64,
    /// What was resident when it opened. A window during which this changes
    /// is abandoned — see the module docs.
    shape: Shape,
    /// Cleared the moment the shape changes; a spoiled window is kept open
    /// (so it is not re-opened and re-spoiled every tick) and dropped when it
    /// closes.
    clean: bool,
}

/// What is on the card besides the window's own pipeline, as far as the
/// registry and the audio residency can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shape {
    /// Every container that is starting or ready.
    containers: Vec<(Class, String, RuntimeState)>,
    /// Which ready audio containers hold their model ([`super::residency`]).
    /// audio.cpp loads a model on its first request while its container
    /// stays `ready`, so an audio load changes nothing above — and is a rise
    /// of 1–3 GB on the same device-wide figure this sampler reads. A window
    /// during which one loads (or idles out) is abandoned like one during
    /// which a container started.
    audio: AudioShape,
}

/// The audio half of a window's [`Shape`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct AudioShape {
    /// `(generation, holds its model)` per ready audio container.
    held: Vec<(u64, bool)>,
    /// One of them is loading its model right now: a request in flight on a
    /// container that has not answered one yet. That moves `used` for the
    /// whole window it overlaps, whether or not it finishes inside it.
    loading: bool,
}

impl AudioShape {
    /// Every ready audio container in `entries`, by the residency's own rule
    /// for whether it holds its model ([`super::residency::holds`]).
    fn of(state: &SharedState, entries: &[RuntimeView]) -> Self {
        let live: Vec<u64> = entries.iter().map(|e| e.generation).collect();
        let gens = state.vram.residency.view(&live);
        let snap = state.snapshot();
        let now = std::time::Instant::now();
        let mut shape = Self::default();
        for e in entries
            .iter()
            .filter(|e| e.class == Class::Audio && e.state == RuntimeState::Ready)
        {
            let answered = gens.get(&e.generation).and_then(|g| g.last_answered);
            let holds = super::residency::holds(answered, now, &snap.settings.audio);
            shape.held.push((e.generation, holds));
            if !holds && e.in_flight > 0 {
                shape.loading = true;
            }
        }
        shape.held.sort_unstable();
        shape
    }
}

/// The per-row sampler state. One of these lives in the task
/// [`run`] spawns; tests drive [`PeakSampler::tick`] by hand.
#[derive(Debug, Default)]
pub struct PeakSampler {
    rows: Mutex<HashMap<String, Row>>,
}

impl PeakSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// One pass. Returns how long to wait before the next one.
    ///
    /// Everything it can do without the driver it does first: when no image
    /// container is up there is no baseline to keep and no window to be in, so
    /// the pass costs one registry map read and NVML is not touched.
    pub async fn tick(&self, state: &SharedState) -> Duration {
        let entries = state.runtime().list();
        let image: Vec<_> = entries
            .iter()
            .filter(|e| {
                e.class == Class::Image
                    && matches!(e.state, RuntimeState::Starting | RuntimeState::Ready)
            })
            .collect();
        if image.is_empty() {
            self.rows.lock().unwrap().clear();
            return IDLE_INTERVAL;
        }

        let Ok(devices) = state.vram.devices().await else {
            // No telemetry: nothing measured, so nothing learned and nothing
            // half-learned either — every open window is dropped rather than
            // closed against a stale baseline.
            self.rows.lock().unwrap().clear();
            return IDLE_INTERVAL;
        };
        if devices.is_empty() {
            self.rows.lock().unwrap().clear();
            return IDLE_INTERVAL;
        }
        // Summed across devices, exactly as the ledger sums their free bytes:
        // one number for the box, because that is the granularity admission
        // decides at.
        let used: u64 = devices.iter().map(|d| d.used_bytes).sum();
        let up: Vec<_> = entries
            .iter()
            .filter(|e| matches!(e.state, RuntimeState::Starting | RuntimeState::Ready))
            .cloned()
            .collect();
        let shape = Shape {
            containers: up
                .iter()
                .map(|e| (e.class, e.model_id.clone(), e.state))
                .collect(),
            audio: AudioShape::of(state, &up),
        };
        // An audio model loading right now is moving `used` for the whole
        // window it overlaps, start to end, whether or not it finishes
        // inside it.
        let audio_loading = shape.audio.loading;

        // model_id -> the delta that window ended on. Collected under the lock
        // and written outside it: the store is async and this is a `Mutex`.
        let mut learned: Vec<(String, u64)> = Vec::new();
        let in_flight = image.iter().any(|e| e.in_flight > 0);
        {
            let mut rows = self.rows.lock().unwrap();
            rows.retain(|id, _| image.iter().any(|e| &e.model_id == id));
            for e in &image {
                let row = rows.entry(e.model_id.clone()).or_default();
                if e.state == RuntimeState::Starting {
                    // The weights are going onto the card right now, so this
                    // reading is neither an idle residency nor a job's peak.
                    row.baseline = None;
                    row.baseline_shape = None;
                    row.window = None;
                    continue;
                }
                if e.in_flight > 0 {
                    match &mut row.window {
                        Some(w) => {
                            if w.shape != shape || audio_loading {
                                w.clean = false;
                            }
                            w.max_used = w.max_used.max(used);
                        }
                        None => {
                            // Against an earlier tick's baseline only when
                            // that tick saw what this one sees, with no
                            // audio load in progress (WP7 review M2).
                            let baseline_matches = match &row.baseline_shape {
                                Some(b) => *b == shape && !b.audio.loading,
                                None => true,
                            };
                            row.window = Some(Window {
                                baseline: row.baseline,
                                max_used: used,
                                shape: shape.clone(),
                                clean: !audio_loading && baseline_matches,
                            })
                        }
                    }
                    continue;
                }
                if let Some(w) = row.window.take() {
                    // This very sample is the idle one, so it is what a window
                    // that opened without a baseline is measured against —
                    // and only when it saw what the window saw.
                    let delta = w.max_used.saturating_sub(w.baseline.unwrap_or(used));
                    let clean = w.clean && (w.baseline.is_some() || w.shape == shape);
                    if clean && delta > 0 {
                        learned.push((e.model_id.clone(), delta));
                    } else if !clean {
                        tracing::debug!(
                            "image/{}: the in-flight window is not usable — something else \
                             started or stopped on the GPU while it was open",
                            e.model_id
                        );
                    }
                }
                row.baseline = Some(used);
                row.baseline_shape = Some(shape.clone());
            }
        }

        for (model_id, delta) in learned {
            self.store(state, &model_id, delta).await;
        }
        if in_flight {
            IN_FLIGHT_INTERVAL
        } else {
            RESIDENT_INTERVAL
        }
    }

    /// Raise the stored peak, never lower it, and reload the snapshot the
    /// ledger reads.
    async fn store(&self, state: &SharedState, model_id: &str, delta: u64) {
        let snap = state.snapshot();
        let Some(row) = snap.image_models.iter().find(|m| m.model_id == model_id) else {
            return;
        };
        if row.peak_extra_bytes.is_some_and(|stored| stored >= delta) {
            return;
        }
        let previous = row.peak_extra_bytes;
        if let Err(e) = crate::store::set_image_model_peak(&state.db, row.id, Some(delta)).await {
            tracing::warn!("could not record the learned peak of image/{model_id}: {e}");
            return;
        }
        if let Err(e) = state.reload_snapshot().await {
            tracing::warn!("learned peak of image/{model_id} recorded but not reloaded: {e}");
            return;
        }
        tracing::info!(
            "image/{model_id}: one generation needed {} above its idle residency{} — admission \
             now keeps that much free while it is resident",
            fmt_bytes(delta),
            match previous {
                Some(p) => format!(" (was {})", fmt_bytes(p)),
                None => String::new(),
            }
        );
        // The resident's note and the free figure both change with it, and no
        // request has to run for the dashboard to be told.
        super::broadcast(state);
    }
}

/// The background task, spawned beside the idle reaper.
pub async fn run(state: SharedState) {
    let sampler = PeakSampler::new();
    loop {
        let next = sampler.tick(&state).await;
        tokio::time::sleep(next).await;
    }
}
