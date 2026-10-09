//! The learned residency of an audio model (realtime design §9.4, WP7).
//!
//! **The gap.** The audio class is charged its model directory's size on
//! disk ([`super::plan::PlanCache::audio`]) — audio.cpp publishes no model
//! metadata to plan from — and a container holds more than that once it has
//! served a request: the CUDA context and ggml's compute buffers come on top
//! of the weights. Measured per process on the 4090: nemotron-asr 1.48 GB
//! against 0.93 GB on disk, qwen3-asr 3.28 against 2.47, pocket-tts 0.97
//! against 0.13. The ratios (1.3×, 1.6×, 7.5×) rule out any multiplier, so
//! the figure is learned rather than guessed — the image class's rule
//! ([`super::peak`]).
//!
//! **Lazy loading makes it worse than a low estimate.** audio.cpp registers a
//! model at start and loads its weights on the first request
//! (`AudioSettings::lazy_load`, on by default), and its readiness route
//! answers before that load. A container that is up but has never been asked
//! anything — a boot warm start, the operator's Start, a voice session's
//! warm until its warm-up request is answered (`realtime::warm`) — holds
//! only its CUDA context: its reservation is gone, the driver
//! does not show its weights, and the ledger would offer that memory to the
//! next start. So a ready audio container that has not loaded yet is charged
//! what it is still to take as **pending** ([`pending`]), next to the other
//! memory the driver cannot see yet (the ledger's `unmeasured` and
//! `reserved_peaks`): its expected residency minus what it holds at rest
//! ([`ready`], WP7 review M4), or all of it where that could not be read.
//!
//! **What is learned, and when.** `resident_bytes`: the largest GPU memory
//! the driver lists for the container's processes (its init and every process
//! under it, [`super::attribution`]), read right after the container answered
//! an inference request with a 2xx ([`LocalHold::note_inference`]) and, until
//! the figure settles, sampled while each request runs ([`sample`]) —
//! audio.cpp frees part of its compute buffers once it has answered. No
//! sampler between requests, at most one per container generation, and a
//! container that is never asked anything costs one reading at rest. Monotone:
//! a smaller reading never lowers the figure. Nothing is learned without
//! per-process figures — no device-wide fallback, which would charge one
//! audio model for everything else on the card — and nothing is ever stored
//! as 0.
//!
//! **Keyed, not reset.** The figure belongs to the configuration it was read
//! under ([`resident_key`]); a row whose configuration changed keeps the
//! figure but is charged at its on-disk size again, and every surface says it
//! was learned for a previous configuration. The key is taken from the row a
//! container was **started** with (the registry entry's `resident_key`), so
//! a container still running a previous configuration — an edit while it was
//! busy, a class image changed without a restart — never teaches the new one,
//! and is charged the figure of the configuration it runs ([`expected_for`]).
//!
//! [`LocalHold::note_inference`]: super::LocalHold::note_inference

use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{AudioModel, AudioSettings};
use crate::hf::fmt_bytes;

use super::plan::Footprint;

mod learn;
mod read;
mod ready;
mod sample;
#[cfg(test)]
mod tests;
mod tracker;
mod weights;

pub use sample::SAMPLE_INTERVAL;
pub(super) use tracker::{AtRest, GenView, Residency};
pub(super) use weights::selected_weights_bytes;

/// The most an audio.cpp container that has loaded no model is taken to hold:
/// its CUDA context and runtime. Used by the at-rest rule ([`looks_loaded`]).
///
/// Where it comes from: a bare container's context was what the WP7 review
/// found double-counted — 0.5–1 GB for a realtime warm-up of two or three
/// containers, so 0.25–0.5 GB each — and the smallest loaded container
/// measured, pocket-tts-english, held 768 MiB (§3.1). Half the row's on-disk
/// size on top keeps the line clear of a larger model's weights. The WP7
/// live test checks it against the real containers: every bare reading must
/// fall below it and every loaded one above.
pub const BARE_CONTEXT_CEILING: u64 = 512 * 1024 * 1024;

/// The at-rest rule (WP7 review M4): a container read at rest holds its
/// model when its processes hold more than [`BARE_CONTEXT_CEILING`] plus
/// half the row's on-disk size. A lazy row's container that does is taken as
/// loaded — the case is a container a previous lmgw left running, adopted
/// at boot after it had answered requests — and an eager row's has its
/// weights on the card, as it should once ready. One that does not holds
/// only a bare context, which is what stays on the card while its model is
/// not loaded.
pub fn looks_loaded(at_rest: u64, on_disk: u64) -> bool {
    at_rest > BARE_CONTEXT_CEILING.saturating_add(on_disk / 2)
}

/// How many sampled stretches in a row must leave a configuration's figure
/// where it was before its requests are no longer sampled ([`sample`]). A
/// stretch is one request, or a realtime response's clauses under one hold.
/// Three: the first stretch teaches the transient, and three that add
/// nothing make a missed larger one unlikely; a larger job later still
/// raises the figure through the reading after its answer.
pub const SETTLE_AFTER: u32 = 3;

/// The configuration an audio model's residency belongs to: the first 12 hex
/// digits of the SHA-256 of a canonical JSON of everything that changes what
/// audio.cpp loads onto the card.
///
/// In: family, path, task, mode, the config and weight ids, the spec
/// override, load and session options (quantization lives there), the
/// effective container image and run args (a CPU-only image, a device
/// filter), the backend in effect (the row's own, else the class's — every
/// row that inherits keeps the key it had before rows could set one, and a
/// row switched to the CPU and back is charged its old figure again) and
/// the class device. Out, because they change nothing
/// that is loaded: voice presets, default request options, busy timeout,
/// lazy loading (when, not what), idle unload, threads, warm start, the hold
/// fallback and `enabled`. A rebuilt image under the same tag is not
/// detected; the running maximum corrects such a figure upward, and the
/// owner's reset (`clear: "residency"`) downward.
pub fn resident_key(m: &AudioModel, s: &AudioSettings) -> String {
    let doc = serde_json::json!({
        "family": m.family,
        "path": m.path,
        "task": m.task,
        "mode": m.mode,
        "config_id": m.config_id,
        "weight_id": m.weight_id,
        "model_spec_override": m.model_spec_override,
        "load_options": m.load_options,
        "session_options": m.session_options,
        "image": m.image.as_deref().unwrap_or(&s.image),
        "extra_run_args": m.extra_run_args.as_ref().unwrap_or(&s.extra_run_args),
        "backend": crate::runtime::audio::backend_in_effect(m, s),
        "device": s.device,
    });
    let digest = Sha256::digest(canonical(&doc).as_bytes());
    hex::encode(&digest[..6])
}

/// JSON with every object's keys sorted, at every depth — whatever order the
/// owner's options were written in, and whether or not `serde_json`'s maps
/// keep insertion order in this build.
fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&m[k])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}

/// The learned figure, when the row has one for its current configuration.
pub fn learned(m: &AudioModel, s: &AudioSettings) -> Option<u64> {
    learned_for(m, &resident_key(m, s))
}

/// The learned figure of the configuration `key`, when it is the one stored.
fn learned_for(m: &AudioModel, key: &str) -> Option<u64> {
    m.residency
        .as_ref()
        .filter(|r| r.key == key)
        .map(|r| r.bytes)
}

/// What an audio row is charged: its learned residency when the row has one
/// for its current configuration, otherwise the on-disk footprint as it is.
/// `weights_bytes` stays the on-disk size either way.
pub(super) fn expected(fp: Footprint, m: &AudioModel, s: &AudioSettings) -> Footprint {
    expected_for(fp, m, s, None)
}

/// [`expected`] for a container that runs the configuration `running` (its
/// registry entry's key): the figure learned for **that** configuration when
/// it is the one stored, even after a class-level change re-keyed the row
/// while the container kept running the old one (WP7 review, low). `None`:
/// the row's current configuration.
pub(super) fn expected_for(
    fp: Footprint,
    m: &AudioModel,
    s: &AudioSettings,
    running: Option<&str>,
) -> Footprint {
    let current = resident_key(m, s);
    let key = running.unwrap_or(&current);
    match learned_for(m, key) {
        Some(bytes) => Footprint {
            total_bytes: bytes,
            note: Some(if key == current {
                format!(
                    "learned residency: its container held {} once it had served a request ({} \
                     on disk)",
                    fmt_bytes(bytes),
                    fmt_bytes(fp.weights_bytes)
                )
            } else {
                format!(
                    "learned residency of the configuration its container still runs: {} ({} \
                     on disk)",
                    fmt_bytes(bytes),
                    fmt_bytes(fp.weights_bytes)
                )
            }),
            ..fp
        },
        None => fp,
    }
}

/// Whether the row loads its model at container start rather than on its
/// first request: its own `lazy`, else the class's `lazy_load`.
pub(super) fn is_eager(m: &AudioModel, s: &AudioSettings) -> bool {
    !m.lazy.unwrap_or(s.lazy_load)
}

/// Whether a container that answered its last inference request at
/// `last_answered` still holds its model at `now` (WP7 review M3): it has
/// answered one, and audio.cpp's `idle_unload_ms` — when set — has not
/// passed since. A request in flight does not count as holding: an unloaded
/// model's next request is a reload, and until it is answered that load is
/// pending like the first one.
pub fn holds(last_answered: Option<Instant>, now: Instant, s: &AudioSettings) -> bool {
    last_answered.is_some_and(|t| {
        s.idle_unload_ms <= 0
            || now.saturating_duration_since(t) < Duration::from_millis(s.idle_unload_ms as u64)
    })
}

/// Why a ready audio container's [`Pending`] figure is what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Basis {
    /// Its model is loaded: nothing is pending.
    Holds,
    /// Read at rest ([`ready`]): it holds `on_card` already, of which the
    /// expected residency is the rest.
    AtRest { on_card: u64, eager: bool },
    /// audio.cpp unloaded it after `idle_unload_ms`; `bare` is what it held
    /// at rest before its first load, when that was read.
    Unloaded { bare: Option<u64> },
    /// A lazy row's container nothing was read of: all of it is pending.
    Whole,
    /// An eager row's container nothing was read of: what comes on top of
    /// its selected weights file ([`weights`]).
    AboveWeights { weights: u64 },
}

/// What a ready audio container will still take once it is asked to work,
/// and the driver cannot show yet ([`pending`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Pending {
    pub(super) bytes: u64,
    pub(super) basis: Basis,
}

/// The pending table. `expected` is the container's expected residency
/// ([`expected_for`]); `weights` its selected weights file, for an eager row
/// nothing was read of. A figure read at rest is what is on the card
/// already, so only the rest is pending — never more than nothing.
pub(super) fn pending(
    m: &AudioModel,
    s: &AudioSettings,
    expected: u64,
    weights: u64,
    g: &GenView,
    now: Instant,
) -> Pending {
    let take = |on_card: u64| expected.saturating_sub(on_card);
    if holds(g.last_answered, now, s) {
        return Pending {
            bytes: 0,
            basis: Basis::Holds,
        };
    }
    if g.last_answered.is_some() {
        let bare = match g.at_rest {
            AtRest::Bare(b) => Some(b),
            _ => None,
        };
        return Pending {
            bytes: take(bare.unwrap_or(0)),
            basis: Basis::Unloaded { bare },
        };
    }
    match g.at_rest {
        AtRest::Bare(b) | AtRest::Eager(b) | AtRest::Loaded(b) => Pending {
            bytes: take(b),
            basis: Basis::AtRest {
                on_card: b,
                eager: is_eager(m, s),
            },
        },
        _ if is_eager(m, s) => Pending {
            bytes: take(weights),
            basis: Basis::AboveWeights { weights },
        },
        _ => Pending {
            bytes: expected,
            basis: Basis::Whole,
        },
    }
}

/// Where a row's current configuration stands with the sampler ([`sample`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Sampling {
    /// Sampled stretches in a row that did not raise the figure.
    pub calm: u32,
}

/// What a row's residency figure is and what it is charged, in one
/// sentence — the models page's note, and the first half of a resident's.
/// `cannot_learn`: why this host cannot learn one (the probe's own words).
pub fn residency_sentence(
    m: &AudioModel,
    s: &AudioSettings,
    on_disk: u64,
    cannot_learn: Option<&str>,
    sampling: Sampling,
) -> String {
    let relearn = |until: &str| match cannot_learn {
        Some(why) => format!("cannot learn: {why}"),
        None => until.to_string(),
    };
    match (&m.residency, learned(m, s)) {
        (Some(r), Some(_)) => format!(
            "resident {} learned {}, charged at admission{}",
            fmt_bytes(r.bytes),
            r.learned_at,
            match cannot_learn {
                Some(_) => String::new(),
                None if sampling.calm >= SETTLE_AFTER => format!(
                    "; settled — {SETTLE_AFTER} sampled requests in a row did not raise it, so \
                     only a reading after each answer is taken now"
                ),
                None => format!(
                    "; its requests are sampled while they run until {SETTLE_AFTER} in a row do \
                     not raise it ({} so far)",
                    sampling.calm
                ),
            }
        ),
        (Some(r), None) => format!(
            "{} was learned for a previous configuration; charged at the on-disk {} — {}",
            fmt_bytes(r.bytes),
            fmt_bytes(on_disk),
            relearn("its next request teaches this one")
        ),
        (None, _) => format!(
            "residency not learned, charged at the on-disk {}, which audio.cpp has been \
             measured to exceed (CUDA context, compute buffers) — {}",
            fmt_bytes(on_disk),
            relearn("its first request teaches it")
        ),
    }
}

/// The sentence for a reading after an answer that found no figure.
pub(super) fn failure_note(why: &str) -> String {
    format!(
        "The last reading after an answer found no figure ({why}); the next answered request \
         reads again"
    )
}

/// The parts of a ready audio resident's note beyond the row's sentence.
pub(super) struct NoteFacts<'a> {
    pub(super) on_disk: u64,
    pub(super) pending: &'a Pending,
    pub(super) cannot_learn: Option<&'a str>,
    pub(super) sampling: Sampling,
    pub(super) gen: &'a GenView,
    /// The container runs a previous configuration whose figure is stored,
    /// and is charged that ([`expected_for`]).
    pub(super) runs_previous: bool,
}

/// A ready audio resident's note: [`residency_sentence`], then what is kept
/// free for a model that has not loaded yet and why that much.
pub(super) fn resident_note(m: &AudioModel, s: &AudioSettings, f: &NoteFacts<'_>) -> String {
    let mut out = residency_sentence(m, s, f.on_disk, f.cannot_learn, f.sampling);
    if f.runs_previous {
        out.push_str(
            ". Its container still runs the configuration that figure was learned for, so it \
             is charged that until it restarts",
        );
    }
    let p = fmt_bytes(f.pending.bytes);
    let unread = || match (&f.gen.at_rest, f.cannot_learn) {
        (_, Some(why)) => format!("this host gives no per-process figures ({why})"),
        (AtRest::Failed(why), _) => format!("what it holds at rest could not be read: {why}"),
        (AtRest::Reading, _) => "what it holds at rest is being read".to_string(),
        _ => "what it held at rest was not read before a request came".to_string(),
    };
    match &f.pending.basis {
        Basis::Holds => {
            if let AtRest::Loaded(b) = f.gen.at_rest {
                out.push_str(&format!(
                    ". Taken as loaded: its processes held {} at rest, more than a bare context \
                     ({} plus half its on-disk size)",
                    fmt_bytes(b),
                    fmt_bytes(BARE_CONTEXT_CEILING)
                ));
            }
        }
        Basis::AtRest {
            on_card,
            eager: false,
        } => out.push_str(&format!(
            ". Not loaded yet: {p} kept free until its first request (it holds {} at rest \
             already)",
            fmt_bytes(*on_card)
        )),
        Basis::AtRest {
            on_card,
            eager: true,
        } => out.push_str(&format!(
            ". Weights loaded, not run yet: {p} more kept free until its first request (it \
             holds {} at rest)",
            fmt_bytes(*on_card)
        )),
        Basis::Unloaded { bare } => out.push_str(&format!(
            ". Unloaded after idling {} ms (idle_unload_ms): {p} kept free until its next \
             request answers{}",
            s.idle_unload_ms,
            match bare {
                Some(b) => format!(" ({} of it held at rest stays on the card)", fmt_bytes(*b)),
                None => String::new(),
            }
        )),
        Basis::Whole if f.pending.bytes > 0 => out.push_str(&format!(
            ". Not loaded yet: {p} kept free until its first request — all of it, because {}",
            unread()
        )),
        Basis::Whole => {}
        Basis::AboveWeights { weights } => out.push_str(&format!(
            ". Weights loaded, not run yet: {p} more kept free until its first request (above \
             its {} weights file, because {})",
            fmt_bytes(*weights),
            unread()
        )),
    }
    if let Some(why) = &f.gen.failure {
        out.push_str(". ");
        out.push_str(&failure_note(why));
    }
    out
}

impl super::VramScheduler {
    /// How many audio residency readings have finished since this scheduler
    /// was made, whatever each found — a counter, never reset, so a test can
    /// tell "the reading after that request is over" without timing it (the
    /// way [`Self::waits_begun`](super::VramScheduler::waits_begun) does for
    /// the queue).
    pub fn residency_readings(&self) -> u64 {
        self.residency
            .readings
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many readings at rest have finished ([`ready`]), the same kind of
    /// counter.
    pub fn residency_at_rest_readings(&self) -> u64 {
        self.residency
            .at_rest_done
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many sampler stretches have ended, their figure stored or not
    /// ([`sample`]), the same kind of counter.
    pub fn residency_stretches(&self) -> u64 {
        self.residency
            .stretches
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many reads the samplers have finished, whatever each found
    /// ([`sample`]), the same kind of counter: a test's fake driver holds a
    /// request's transient until a sampler has read it, rather than for a
    /// time a loaded box can outlast.
    pub fn residency_samples(&self) -> u64 {
        self.residency
            .samples
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many tombstones the PID cache keeps — processes of containers
    /// that left, until the driver stops listing them. A count for the tests
    /// that pin that they are settled.
    pub fn tombstones(&self) -> usize {
        self.pids.lock().unwrap().tombstone_pids().len()
    }
}
