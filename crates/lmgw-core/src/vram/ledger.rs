//! The ledger

use std::collections::HashSet;

use crate::config::Snapshot;
use crate::runtime::registry::{RuntimeState, RuntimeView};
use crate::runtime::Class;
use crate::state::SharedState;

use super::attribution::{self, Container, Fill, Member, Reading};
use super::charge::CardCharge;
use super::nvml::{DeviceId, DeviceProcesses, GpuMemory, ProcessMemory};
use super::on_cpu::on_gpu;
use super::plan::image::CONTEXT_STAND_IN;
use super::residency;
use super::scheduler::{Reservation, CONTROL_TIMEOUT, MIB};
use super::view::{peak_note, KeptFree, ResidentView};
use super::{broadcast, VramScheduler};
use crate::bench::OnCard;

#[derive(Clone)]
pub(super) struct Capacity {
    pub(super) total: u64,
    pub(super) free: u64,
    /// True when `free` came from the driver rather than from the estimate.
    pub(super) measured: bool,
    /// What `free` was lowered by on top of the memory in use: the parts
    /// the driver cannot show yet.
    pub(super) kept_free: KeptFree,
}

pub(super) struct Ledger {
    pub(super) devices: Vec<GpuMemory>,
    pub(super) telemetry_error: Option<String>,
    pub(super) residents: Vec<ResidentView>,
    /// The registry rows the residents were built from — carries the host port
    /// and the raw `last_used`, which eviction needs and the published view
    /// deliberately does not grow a field for.
    pub(super) entries: Vec<RuntimeView>,
    pub(super) estimated_resident: u64,
    pub(super) capacity: Option<Capacity>,
    pub(super) inactive_reason: Option<String>,
    /// What [`Capacity::kept_free`] holds, by the cards each part of it is
    /// charged on (device-ledger design WP4): a card's own figure where no
    /// group is on it ([`Self::cards`]).
    pub(super) kept_on: KeptOn,
    /// The declared budget (`vram.budget_mb`), in bytes; 0 for none
    /// (device-ledger design §9.2).
    pub(super) budget: u64,
    /// The claims of starts that wait for their cards (§5.5), as this read
    /// found them.
    pub(super) claims: Vec<super::scheduler::Claim>,
    /// What each entry's row costs, by generation, as this read sized it
    /// for the charge: where a model stopped for room could be started
    /// again (device-ledger design §6.2) is asked of it.
    pub(super) footprints: Vec<(u64, super::Footprint)>,
}

/// A llama-server adopted from an earlier lmgw that has served nothing in
/// this one: its first-prompt margin stays unless what it holds is read.
fn adopted_unserved(e: &RuntimeView) -> bool {
    e.adopted
        && !e.served
        && e.state == RuntimeState::Ready
        && matches!(e.class, Class::Chat | Class::Aux)
}

impl VramScheduler {
    /// Assemble the ledger: measure, then account, then reconcile.
    pub(super) async fn ledger(&self, state: &SharedState, snap: &Snapshot) -> Ledger {
        let s = &snap.settings;
        // What the engine costs on each card, as stored (§4.2).
        self.load_engine_costs(&state.db).await;

        // Residency is the registry, not a question asked over HTTP: an entry
        // exists exactly while lmgw believes a container for that model is up,
        // and `starting` counts because the weights are already going onto the
        // card. A container on the CPU is no part of it (`on_cpu`): this one
        // filter keeps it out of the charge, the pending loads, eviction, the
        // holders a refusal names and the hold's draining list.
        let entries: Vec<RuntimeView> = state
            .runtime()
            .list()
            .into_iter()
            .filter(|e| matches!(e.state, RuntimeState::Starting | RuntimeState::Ready))
            .filter(on_gpu)
            .collect();
        let (reservations, claims) = {
            let live = self.live.lock().unwrap();
            (live.reservations.clone(), live.claims.clone())
        };

        let mut residents: Vec<ResidentView> = Vec::new();
        // What is committed but cannot be in the driver's `used` figure yet: a
        // container still uploading weights, and a reservation whose container
        // has not been created at all. Subtracted from the measured free below
        // — conservatively, since a half-loaded model is counted whole.
        let mut unmeasured: u64 = 0;
        // The other thing the driver's `used` cannot show: what a resident
        // image pipeline will allocate the moment it is asked to draw
        // (image-generation §9). Measured per row by [`peak`] and charged here
        // for exactly as long as the pipeline is on the card, because a
        // generation is one request away the whole time. Its sibling above is
        // memory that is *arriving*; this is memory that will be *taken back*.
        let mut reserved_peaks: u64 = 0;
        // And what an image pipeline whose weights wait off the card may
        // still put there: a partial start's grant, or the whole charge of a
        // pipeline whose weights all wait in RAM or on disk, less what its
        // processes hold now ([`super::partial`]). Like the audio loads below,
        // only where the driver is the measurement: in the budget-only branch
        // the estimate is that charge already.
        let mut partial_kept: u64 = 0;
        // And the third (realtime design §9.4): what a ready audio container
        // will load on its first request — audio.cpp loads lazily, and its
        // readiness route answers before the weights are on the card. Like
        // the peaks, charged only where the driver is the measurement: in the
        // budget-only branch `estimated_resident` already counts the whole
        // expected residency.
        let mut pending_loads: u64 = 0;
        // The four above, by the cards each part is charged on.
        let mut kept_on = KeptOn::default();
        let mut footprints: Vec<(u64, super::Footprint)> = Vec::new();
        // Pruned against every live container, the CPU's too: what a CPU
        // container has answered is all its residency knows (`audio_model_
        // loaded`), and forgetting it here made it read as never loaded — a
        // reload sent at each warm, and a "cold" stage on every turn.
        let live: Vec<u64> = state
            .runtime()
            .list()
            .into_iter()
            .filter(|e| matches!(e.state, RuntimeState::Starting | RuntimeState::Ready))
            .map(|e| e.generation)
            .collect();
        let gens = self.residency.view(&live);
        let now = std::time::Instant::now();
        let cannot_learn = entries
            .iter()
            .any(|e| e.class == Class::Audio)
            .then(|| self.probe().process_support().err())
            .flatten();
        // What every ready audio container lmgw does not know to hold its
        // model holds now, read around the device read itself
        // ([`residency::now`]): a load lmgw did not route is on the card and
        // in `used` already. Only where per-process figures exist at all —
        // otherwise nothing is asked, and the notes say why as before.
        let asked: Vec<residency::Ask> = match cannot_learn {
            Some(_) => Vec::new(),
            None => {
                let pids = self.pids.lock().unwrap();
                entries
                    .iter()
                    .filter(|e| e.class == Class::Audio && e.state == RuntimeState::Ready)
                    .filter(|e| {
                        let held = gens.get(&e.generation).and_then(|g| g.last_answered);
                        !residency::holds(held, now, &s.audio)
                    })
                    .map(|e| residency::Ask {
                        generation: e.generation,
                        root: pids.root(e.generation),
                    })
                    .collect()
            }
        };
        // What those image containers hold now, read the same way: only
        // where per-process figures exist, from the PID cache, nothing
        // waited for.
        let mut asked = asked;
        if entries.iter().any(|e| off_card_image(snap, e)) && self.probe().process_support().is_ok()
        {
            let pids = self.pids.lock().unwrap();
            asked.extend(entries.iter().filter(|e| off_card_image(snap, e)).map(|e| {
                residency::Ask {
                    generation: e.generation,
                    root: pids.root(e.generation),
                }
            }));
        }
        // And what each adopted llama-server that has served nothing in this
        // lmgw holds now: a reading of it is a measurement of its footprint,
        // so it keeps no first-prompt margin (`headroom`).
        if entries.iter().any(adopted_unserved) && self.probe().process_support().is_ok() {
            let pids = self.pids.lock().unwrap();
            asked.extend(
                entries
                    .iter()
                    .filter(|e| adopted_unserved(e))
                    .map(|e| residency::Ask {
                        generation: e.generation,
                        root: pids.root(e.generation),
                    }),
            );
        }
        // And what each running model charged on several cards holds on
        // each of them (device-ledger design §3.3, §4.3, WP5, WP6): a group,
        // or a split, that runs is charged by what its processes hold on
        // each card, where the driver says so.
        if self.probe().process_support().is_ok() {
            let last = self.last_devices.lock().unwrap().clone();
            let groups: Vec<u64> = entries
                .iter()
                .filter(|e| e.state == RuntimeState::Ready)
                .filter(|e| !asked.iter().any(|a| a.generation == e.generation))
                .filter(|e| self.entry_cards(snap, e, &last).charge.on_several())
                .map(|e| e.generation)
                .collect();
            let pids = self.pids.lock().unwrap();
            asked.extend(groups.into_iter().map(|generation| residency::Ask {
                generation,
                root: pids.root(generation),
            }));
        }
        let read = self.devices_and_held(asked).await;
        let held_now = read.held;
        let held_cards = read.cards;
        let (devices, telemetry_error) = match read.devices {
            Ok(d) => (d, None),
            Err(e) => (vec![], Some(e)),
        };
        if !devices.is_empty() {
            self.device_total
                .store(pooled(0, &devices).0, std::sync::atomic::Ordering::Relaxed);
            self.device_count
                .store(devices.len(), std::sync::atomic::Ordering::Relaxed);
            *self.last_devices.lock().unwrap() = devices.clone();
            // The cards the headroom is kept on (device-ledger design §9.1).
            self.growth.lock().unwrap().set_present(&devices);
            // The audio class's device index, read once as a pin (§8.1).
            self.audio_device_once(state, snap, &devices);
        }
        for e in &entries {
            // The cards it is charged on (device-ledger design §3.2, §3.3).
            let on = self.entry_cards(snap, e, &devices);
            if let Some(gone) = on.nowhere() {
                residents.push(ResidentView {
                    container: e.class,
                    model: e.model_id.clone(),
                    state: e.state.as_str().to_string(),
                    estimated_bytes: 0,
                    charge: CardCharge::none(),
                    in_flight: e.in_flight as usize,
                    idle_seconds: Some(e.last_used_age_seconds),
                    peak_extra_bytes: None,
                    pending_bytes: None,
                    note: Some(gone),
                    cards: Vec::new(),
                    generation: Some(e.generation),
                    kept_bytes: 0,
                });
                continue;
            }
            let cards = on.charge.cards();
            // The rung the container runs, `starting` included: a climb's new
            // rung is charged from its claim on (ladder design §5).
            let fp = self.footprint_of(snap, e).await;
            if let Some(f) = &fp {
                footprints.push((e.generation, f.clone()));
            }
            // A partial start is charged what admission gave it
            // (`registry::admission_grant`), from its claim on.
            let bytes = match &e.grant {
                Some(g) => g.bytes,
                None => fp.as_ref().map(|f| f.total_bytes).unwrap_or(0),
            };
            // And what its engine costs on each card it is on (§4.2): a
            // llama.cpp model's, from the estimate on.
            let bytes = cards
                .iter()
                .map(|c| self.engine_on(e.class, c))
                .fold(bytes, u64::saturating_add);
            // A start is not measured yet: its class's margin rides on its
            // charge in every headroom mode (`headroom`, WP13). A partial
            // image start sets its context aside in its grant.
            let margin = match (&e.grant, fp.as_ref()) {
                (Some(_), _) if e.class == Class::Image => 0,
                (_, Some(f)) => self.margin(snap, e.class, &e.model_id, f),
                (_, None) => 0,
            };
            // A split start is charged its part on each card (§4.3), as
            // admission reserved it, until it is measured once it runs: how
            // a total that moved since would divide among them is not known.
            let split = on.charge.split_parts().is_some();
            let bytes = match split {
                true => on.charge.total(),
                false => bytes,
            };
            if e.state == RuntimeState::Starting {
                unmeasured = unmeasured.saturating_add(bytes).saturating_add(margin);
                kept_on.starting_charge(&on.charge, &cards, bytes, margin);
            }
            // A llama-server that is ready but has served nothing yet: what
            // its first prompt puts on the card (the CUDA pool's
            // temporaries) is not in the driver's figure yet, so its margin
            // stays until a request has run (`headroom`) — except on a
            // container adopted from an earlier lmgw that the driver shows
            // per process: what it holds is read, the measurement its
            // margin stands in for, whatever it served before.
            let measured_adopted =
                e.adopted && matches!(held_now.get(&e.generation), Some(Ok(b)) if *b > 0);
            let first_prompt = matches!(e.class, Class::Chat | Class::Aux)
                && e.state == RuntimeState::Ready
                && !e.served
                && !measured_adopted
                && margin > 0;
            if first_prompt {
                unmeasured = unmeasured.saturating_add(margin);
                kept_on.starting_charge(&on.charge, &cards, 0, margin);
            }
            // Charged from `ready` on, in flight or not. While a generation is
            // actually running the driver's `used` already contains some or
            // all of it, so for those seconds the charge is doubled — left
            // that way deliberately: the alternative is subtracting a figure
            // nobody measured from a figure that is still moving, and the
            // window it spans is the one where being wrong is expensive.
            let peak = match (e.class, e.state) {
                (Class::Image, RuntimeState::Ready) => snap
                    .image_models
                    .iter()
                    .find(|m| m.model_id == e.model_id)
                    .and_then(|m| {
                        super::plan::image::learned_for(m, &self.entry_engine_key(snap, e, m)).peak
                    }),
                _ => None,
            };
            // A pipeline whose weights wait off the card holds little of its
            // charge between generations, and the driver's `used` says so:
            // the rest is what the next generation puts there, kept free
            // (`partial`). What it holds now comes off: read per process,
            // else its measured context, else nothing.
            //
            // - A partial start: its grant, or what sd-server can draw where
            //   that is more — the budget it is held to (the row's own
            //   `max_vram` wins) and its context outside it: measured, else
            //   read at rest where its weights all wait off the card, else
            //   the stand-in.
            // - A whole start of a row whose weights all wait off the card,
            //   with a learned peak: the peak and its context — measured,
            //   else what its processes hold at rest; with neither, its files
            //   and its peak, what admission charged it before it adapted.
            //   Without a peak, its files.
            let reading = match held_now.get(&e.generation) {
                Some(Ok(b)) => Some(*b),
                _ => None,
            };
            let partial = match (e.class, e.state, fp.as_ref()) {
                (Class::Image, RuntimeState::Ready, Some(f)) => match (&e.grant, &f.partial) {
                    (Some(g), p) => {
                        let measured = p.as_ref().and_then(|p| p.context_bytes);
                        // A reading is its context only at rest, and only
                        // where no weights stay on the card between uses.
                        let at_rest =
                            e.in_flight == 0 && p.as_ref().is_some_and(|p| p.how.all_off());
                        let context = measured
                            .or(reading.filter(|_| at_rest))
                            .unwrap_or(CONTEXT_STAND_IN);
                        let row = snap.image_models.iter().find(|m| m.model_id == e.model_id);
                        let budget = match row.and_then(|r| super::partial::own_budget(&r.args)) {
                            Some(Some(own)) => own,
                            // Unbounded: what the whole could put there.
                            Some(None) => f.total_bytes,
                            None => g.max_vram_bytes,
                        };
                        Some((g.bytes.max(budget.saturating_add(context)), measured))
                    }
                    (None, Some(p)) if p.how.all_off() => Some((
                        // A reading is its context only at rest: one taken
                        // while a generation runs holds the peak too.
                        match (
                            peak,
                            p.context_bytes.or(reading.filter(|_| e.in_flight == 0)),
                        ) {
                            (Some(pk), Some(context)) => pk.saturating_add(context),
                            (Some(pk), None) => f.total_bytes.saturating_add(pk),
                            (None, _) => f.total_bytes,
                        },
                        p.context_bytes,
                    )),
                    _ => None,
                },
                _ => None,
            };
            let bytes = partial.map_or(bytes, |(charge, _)| charge);
            match partial {
                Some((charge, context)) => {
                    let holds = reading.or(context).unwrap_or(0);
                    partial_kept = partial_kept.saturating_add(charge.saturating_sub(holds));
                    kept_on.image_peak(&cards, charge.saturating_sub(holds));
                }
                None => {
                    if let Some(p) = peak {
                        reserved_peaks = reserved_peaks.saturating_add(p);
                        kept_on.image_peak(&cards, p);
                    }
                }
            }
            let audio = match (e.class, e.state, fp.as_ref()) {
                (Class::Audio, RuntimeState::Ready, Some(fp)) => snap
                    .audio_models
                    .iter()
                    .find(|m| m.model_id == e.model_id)
                    .map(|row| (row, fp)),
                _ => None,
            };
            let gen = gens.get(&e.generation).cloned().unwrap_or_default();
            let held = held_now.get(&e.generation);
            let pending = match audio {
                Some((row, fp)) => {
                    // Only an eager row nothing was read of needs its
                    // weights file (memoized after the first ledger).
                    let weights = if residency::is_eager(row, &s.audio) {
                        self.plans.audio_weights(&s.audio.models_dir, row).await
                    } else {
                        0
                    };
                    Some(residency::pending(
                        row,
                        &s.audio,
                        fp.total_bytes,
                        weights,
                        &gen,
                        now,
                        held.and_then(|h| h.as_ref().ok().copied()),
                    ))
                }
                None => None,
            };
            // An audio row whose residency is not learned keeps its margin
            // on the load still to come (`headroom`).
            let pending_bytes = pending.as_ref().map(|p| match p.bytes {
                0 => 0,
                b => b.saturating_add(margin),
            });
            pending_loads = pending_loads.saturating_add(pending_bytes.unwrap_or(0));
            kept_on.pending_load(&cards, pending_bytes.unwrap_or(0));
            let note = match (e.class, e.state, audio, &pending) {
                (Class::Image, RuntimeState::Ready, _, _) if partial.is_some() => {
                    Some(format!(
                        "{}. Admission charges it {}{} and keeps free what of that it does not                          hold yet, while it is resident",
                        fp.as_ref()
                            .and_then(|f| f.note.as_deref())
                            .unwrap_or_default(),
                        crate::hf::fmt_bytes(bytes),
                        if e.grant.is_some() {
                            ", what it was given on the card and is held to (--max-vram),"
                        } else {
                            ""
                        }
                    ))
                }
                (Class::Image, RuntimeState::Ready, _, _) => {
                    Some(peak_note(fp.as_ref().and_then(|f| f.note.as_deref()), peak))
                }
                (_, _, Some((row, fp)), Some(pending)) => Some(residency::resident_note(
                    row,
                    &s.audio,
                    &residency::NoteFacts {
                        on_disk: fp.weights_bytes,
                        pending,
                        cannot_learn: cannot_learn.as_deref(),
                        sampling: self.sampling(row, &s.audio),
                        gen: &gen,
                        runs_previous: e.resident_key.as_deref().is_some_and(|k| {
                            k != residency::resident_key(row, &s.audio)
                                && row.residency.as_ref().is_some_and(|r| r.key == k)
                        }),
                        unmeasured: held.and_then(|h| h.as_ref().err()).map(String::as_str),
                    },
                )),
                _ => fp.as_ref().and_then(|f| f.note.clone()),
            };
            // Charged on cards it was not recorded on, or on fewer than it
            // was: the resident says so.
            let note = match on.note.as_deref() {
                None => note,
                Some(cards) => Some(match note {
                    Some(n) => format!("{n}. {cards}"),
                    None => cards.to_string(),
                }),
            };
            let note = match first_prompt {
                false => note,
                true => {
                    let kept = format!(
                        "{} is kept free beside it until its first request has run (what \
                         llama-server's first prompt adds: the CUDA pool's temporaries)",
                        crate::hf::fmt_bytes(margin)
                    );
                    Some(match note {
                        Some(n) => format!("{n}. {kept}"),
                        None => kept,
                    })
                }
            };
            // A group that runs, charged by what its processes hold on each
            // card where the driver says so (WP5): the measurement, in
            // place of its estimate on the cards together.
            let measured_on = match (e.state, held_cards.get(&e.generation)) {
                (RuntimeState::Ready, Some(Ok(c))) if !c.is_empty() && on.charge.on_several() => {
                    Some(c.clone())
                }
                _ => None,
            };
            let (bytes, charge, note) = match measured_on {
                Some(c) => {
                    let charge = CardCharge::per_card(c);
                    let total = charge.total();
                    let said =
                        "Charged what its processes hold on each card they are on".to_string();
                    // In place of the sentence that says it is not.
                    let note = match (note, on.note.as_deref()) {
                        (Some(n), Some(guess)) if n.contains(guess) => n.replace(guess, &said),
                        (Some(n), _) => format!("{n}. {said}"),
                        (None, _) => said,
                    };
                    (total, charge, Some(note))
                }
                None if split => (bytes, on.charge.clone(), note),
                None => (bytes, on.charge.with_total(bytes), note),
            };
            residents.push(ResidentView {
                container: e.class,
                model: e.model_id.clone(),
                state: e.state.as_str().to_string(),
                estimated_bytes: bytes,
                charge,
                in_flight: e.in_flight as usize,
                idle_seconds: Some(e.last_used_age_seconds),
                // A pipeline whose weights wait off the card has its peak in
                // its charge, not on top of it.
                peak_extra_bytes: peak.filter(|_| partial.is_none()),
                pending_bytes: pending_bytes.filter(|p| *p > 0),
                note,
                cards: Vec::new(),
                generation: Some(e.generation),
                kept_bytes: match first_prompt {
                    true => margin,
                    false => 0,
                },
            });
        }
        // A reservation is only charged while nothing in the registry answers
        // for its model — otherwise the same model would be counted twice for
        // the whole length of its own start.
        for r in &reservations {
            if entries
                .iter()
                .any(|e| e.class == r.class && e.model_id == r.model_id)
            {
                continue;
            }
            // Its class's margin too, as a start's (`headroom`); a partial
            // image grant, less than its files, sets its context aside.
            let margin = match self.footprint(snap, r.class, &r.model_id).await {
                Some(f) if r.class == Class::Image && r.charge.total() < f.total_bytes => 0,
                Some(f) => self.margin(snap, r.class, &r.model_id, &f),
                None => 0,
            };
            let bytes = r.charge.total();
            unmeasured = unmeasured.saturating_add(bytes).saturating_add(margin);
            kept_on.starting_charge(&r.charge, &r.charge.cards(), bytes, margin);
            residents.push(ResidentView {
                container: r.class,
                model: r.model_id.clone(),
                state: "reserved".into(),
                estimated_bytes: bytes,
                charge: r.charge.clone(),
                in_flight: 0,
                idle_seconds: None,
                // A reservation is a start that has not happened; its
                // pipeline cannot be asked to draw until it is `ready`, and
                // the reservation itself already charges the idle figure.
                peak_extra_bytes: None,
                pending_bytes: None,
                note: None,
                cards: Vec::new(),
                generation: None,
                kept_bytes: 0,
            });
        }

        let estimated_resident: u64 = residents.iter().map(|r| r.estimated_bytes).sum();
        let budget = s.vram.budget_mb.saturating_mul(MIB);

        // Capacity, in the order of decreasing authority: a declared budget is
        // an explicit instruction, the driver's total is a measurement, and
        // absent both there is nothing to admit against.
        let measured_kept = KeptFree {
            starting_bytes: unmeasured,
            image_peak_bytes: reserved_peaks.saturating_add(partial_kept),
            pending_load_bytes: pending_loads,
        };
        let (capacity, inactive_reason) = match (budget, devices.is_empty()) {
            (0, true) => (
                None,
                Some(format!(
                    "no GPU telemetry ({}) and no GPU memory budget declared ({}) — admission \
                     control has nothing to measure against and is forwarding every request \
                     unchanged",
                    telemetry_error.as_deref().unwrap_or("no devices"),
                    crate::config::BUDGET_LABEL
                )),
            ),
            (0, false) => {
                let (total, free) = pooled(0, &devices);
                (
                    Some(Capacity {
                        total,
                        free: free
                            .saturating_sub(unmeasured)
                            .saturating_sub(reserved_peaks)
                            .saturating_sub(partial_kept)
                            .saturating_sub(pending_loads),
                        measured: true,
                        kept_free: measured_kept,
                    }),
                    None,
                )
            }
            (b, true) => (
                Some(Capacity {
                    total: b,
                    // Everything committed is already in `estimated_resident`
                    // here, reservations included — no second subtraction.
                    // The learned peaks are the exception, and have to be:
                    // `estimated_resident` is what the ledger believes is *on
                    // the card*, the figure a surface compares against the
                    // driver's `used`, and a transient nobody has allocated
                    // yet does not belong in it.
                    // A pipeline whose weights wait off the card has its
                    // whole charge in the estimate (`partial_kept` is the
                    // measured branches' only).
                    free: b
                        .saturating_sub(estimated_resident)
                        .saturating_sub(reserved_peaks),
                    measured: false,
                    kept_free: KeptFree {
                        image_peak_bytes: reserved_peaks,
                        ..KeptFree::default()
                    },
                }),
                None,
            ),
            (b, false) => {
                let (_, raw_free) = pooled(b, &devices);
                (
                    Some(Capacity {
                        total: b,
                        free: raw_free
                            .saturating_sub(unmeasured)
                            .saturating_sub(reserved_peaks)
                            .saturating_sub(partial_kept)
                            .saturating_sub(pending_loads),
                        measured: true,
                        kept_free: measured_kept,
                    }),
                    None,
                )
            }
        };

        Ledger {
            devices,
            telemetry_error,
            residents,
            entries,
            estimated_resident,
            capacity,
            inactive_reason,
            kept_on,
            budget,
            claims,
            footprints,
        }
    }

    /// The card's total as last known — a declared `vram.budget_mb`, else the
    /// devices' total the last ledger read measured — with no device read of
    /// its own; `None` while neither is known (no budget, nothing measured
    /// yet). The same total [`Self::ledger`]'s capacity carries. The measured
    /// one is as old as the last ledger read that saw a device (a failed read
    /// keeps the previous figure); `start_background_at` says why that bound
    /// is enough where it is read.
    pub(super) fn known_total(&self, snap: &Snapshot) -> Option<u64> {
        match snap.settings.vram.budget_mb.saturating_mul(MIB) {
            0 => Some(self.device_total.load(std::sync::atomic::Ordering::Relaxed))
                .filter(|t| *t > 0),
            budget => Some(budget),
        }
    }

    /// §4.7's measurement, per device: lmgw's own share there, measured per
    /// process, and the used memory there, whose sums the decisions read
    /// ([`Measured::shares`]); and the pooled capacity and free memory from
    /// the same device read. `Err` is why the share cannot be
    /// measured right now — the trigger is then unavailable and admission
    /// behaves exactly as it always has.
    ///
    /// Everything that could make the share a low guess is a refusal here: a
    /// container still starting, a reservation for one that has not entered
    /// the registry, a container none of whose processes the driver lists,
    /// a figure the driver cannot give ([`attribution`]'s module docs), and
    /// lmgw's containers changing while it reads.
    ///
    /// **In this order** (review finding 2), because free memory and lmgw's
    /// share have to describe the same moment — something of lmgw's that
    /// shrank between a device read and a process read would be in neither,
    /// and read as outside use:
    /// 1. the registry, and every PID it needs — the only part that may wait
    ///    (a `podman inspect`);
    /// 2. on one blocking thread, back to back: the processes under each
    ///    init, the driver's process list, the devices, the process list
    ///    again — per PID the larger of the two figures, so a process that
    ///    shrank or left around the device read counts at what the device
    ///    read may have seen;
    /// 3. the registry again: any container that appeared, left or changed
    ///    state (or a start reserved) since step 1 is a change the reads may
    ///    have seen half of, and the pass is unavailable.
    pub(super) async fn measure(
        &self,
        state: &SharedState,
        snap: &Snapshot,
        l: &Ledger,
        fill: Fill,
    ) -> Result<Measured, String> {
        if let Some(e) = &l.telemetry_error {
            return Err(format!("no GPU telemetry ({e})"));
        }
        if l.devices.is_empty() {
            return Err("the GPU probe reports no device".into());
        }
        let probe = self.probe();
        probe
            .process_support()
            .map_err(|e| format!("no per-process GPU memory: {e}"))?;

        // 1. lmgw's containers, and their PIDs — a benchmark run's too. Those
        // on the GPU only: a CPU container has no GPU process, and as a member
        // it would make every pass refuse ("running without the GPU").
        let entries = gpu_entries(state);
        let reservations = self.live.lock().unwrap().reservations.clone();
        if let Some(why) = unmeasured_start(&entries, &reservations) {
            return Err(why);
        }
        let bench = bench_container(state)?;
        let before = Roster::of(&entries, &reservations).and_bench(bench_on_card(state));

        let mut current: Vec<Container> = entries.iter().map(container_of).collect();
        current.extend(bench);
        let plan = self.pids.lock().unwrap().plan(&current, fill);
        if let Some(done) = plan.done {
            // One `podman inspect` for every container not known yet, in a
            // task of its own so the answer is recorded even if the caller
            // goes away (a view whose client hung up).
            self.read_pids(state, plan.inspect, done, fill == Fill::View, false);
        }
        if let Some(why) = plan.blocked {
            return Err(why);
        }
        // A verdict waits for its own read and joins any already in flight
        // (review finding 4). Each read is bounded by `CONTROL_TIMEOUT` in its
        // own task; this bound is only the net under that one.
        if !plan.wait.is_empty() {
            let reads = futures::future::join_all(plan.wait.into_iter().map(|r| r.done()));
            if tokio::time::timeout(CONTROL_TIMEOUT, reads).await.is_err() {
                return Err(format!(
                    "a container's PID was still being read after {}s — podman inspect is \
                     that slow to answer (lmgw's timeout for control calls, \
                     vram::CONTROL_TIMEOUT)",
                    CONTROL_TIMEOUT.as_secs()
                ));
            }
        }
        let roots: Vec<(Container, u32)> = {
            let cache = self.pids.lock().unwrap();
            current
                .into_iter()
                .map(|c| cache.root(c.generation).map(|r| (c, r)))
                .collect::<Result<_, _>>()?
        };

        // 2. The processes under each init, then the driver's process list,
        // its devices and its process list again — all blocking reads, on one
        // thread, with nothing in between that could wait.
        let tree = self.procs.read().unwrap().clone();
        let tombstones = self.pids.lock().unwrap().tombstones();
        let read = tokio::task::spawn_blocking(move || {
            let members: Vec<Member> = roots
                .into_iter()
                .map(|(c, root)| Member::measure(c.generation, c.label, root, tree.as_ref()))
                .collect::<Result<_, _>>()?;
            let own: Vec<u32> = members
                .iter()
                .flat_map(|m| m.pids.iter().copied())
                .collect();
            // A tombstone whose PID runs a later process is not asked about.
            let reused = attribution::reused(&tombstones, tree.as_ref());
            let retired: Vec<u32> = tombstones
                .iter()
                .map(|t| t.pid)
                .filter(|p| !reused.contains(p))
                .collect();
            let list = || {
                probe
                    .processes(&own, &retired)
                    .map_err(|e| format!("no per-process GPU memory right now: {e}"))
            };
            let first = list()?;
            let devices = probe
                .devices()
                .map_err(|e| format!("no GPU telemetry ({e})"))?;
            let second = list()?;
            let asked: HashSet<u32> = own.iter().chain(&retired).copied().collect();
            Ok::<_, String>((
                members,
                larger_per_pid(&first, &second),
                devices,
                asked,
                own,
                reused,
            ))
        })
        .await
        .map_err(|e| format!("GPU probe thread failed: {e}"))?;
        let (members, listed, devices, asked, own_pids, reused) = read?;
        if devices.is_empty() {
            return Err("the GPU probe reports no device".into());
        }

        // 3. The registry (and the benchmark's container) again.
        let after = Roster::of(&gpu_entries(state), &self.live.lock().unwrap().reservations)
            .and_bench(bench_on_card(state));
        let mut cache = self.pids.lock().unwrap();
        cache.remember(&members);
        // `after` is read before this lock: a concurrent pass's plan may
        // have retired a member in between, whose processes are tombstones
        // now and would count twice, as a member and as a tombstone
        // (`settle` keeps them, since a retired generation's PIDs no longer
        // drop its own tombstones).
        if after != before || !members.iter().all(|m| cache.is_known(m.generation)) {
            return Err(
                "lmgw's containers changed while measuring — the next verdict measures again"
                    .into(),
            );
        }
        // Per card (device-ledger design §2.3). A card whose figure is
        // unknown is unknown on its own here; the sums the decisions read
        // are unknown with it ([`Measured::shares`]), as the pooled figure
        // was.
        let own = attribution::members_share(&members, &listed)?;
        let stopped = cache.settle(&listed, &asked, &own_pids, &reused);
        drop(cache);
        let budget = snap.settings.vram.budget_mb.saturating_mul(MIB);
        let (total, raw_free) = pooled(budget, &devices);
        Ok(Measured {
            cards: card_shares(&devices, own.into_iter().chain(stopped)),
            members: attribution::members_on_cards(&members, &listed),
            total,
            raw_free,
        })
    }

    /// Read the host PID of every registered container that has none cached
    /// yet — one batched `podman inspect` in a background task, never waited
    /// on (candidate-aliases design §4.7).
    ///
    /// Called when a container becomes `ready` (a start through admission or
    /// the operator surface, a warm start) and after boot adopts what podman
    /// still runs. Without it a container's PID was only learned by the first
    /// verdict or status view that saw it, so one evicted before either had
    /// run left no tombstone, and its process — still on the card for a
    /// moment after `podman wait` — read as outside use. The processes under
    /// the init are read too, so an image container's tombstone holds its
    /// sd-server and not only catatonit.
    ///
    /// Only while the trigger is armed and the probe can list processes:
    /// otherwise no pass would read the PID, and nothing is spawned (§7 item
    /// 14). A `starting` entry is left alone — its PID is not meaningful yet,
    /// and a start in flight makes every pass unavailable anyway. The one
    /// part that is not gated is an audio container's reading at rest
    /// ([`residency`], WP7 review M4), which reads its PID for itself.
    pub fn cache_pids(&self, state: &SharedState) {
        // What a ready audio container holds at rest is read whatever the
        // trigger's switches say ([`residency`]'s at-rest reading).
        self.read_audio_at_rest(state);
        // And what an image container of a row whose weights all wait off
        // the card holds at rest: its context (`partial::floor`).
        self.read_image_at_rest(state);
        // And what a llama.cpp model's engine costs on its card (§4.2).
        self.read_engine_costs(state);
        if trigger_off(&state.snapshot()).is_some() || self.probe().process_support().is_err() {
            return;
        }
        // The status view's rule: ask only for a generation never asked
        // about; one already being read, or whose read failed, is left to
        // the next verdict.
        self.pid_plan(state, Fill::View);
    }

    /// [`Self::cache_pids`] without its gate: plan the PIDs of every
    /// registered container that is not `starting` (and the benchmark's),
    /// spawn the batched `podman inspect` the plan asks for — reading the
    /// processes under each init too — and hand back the reads `fill` waits
    /// for (none for [`Fill::View`]).
    ///
    /// Every container goes into the plan, never only the one a caller is
    /// after: the cache retires whatever a plan does not name into
    /// tombstones. The audio residency reading is the caller that needs no
    /// gate ([`residency`]): it learns a figure about a row, whatever the
    /// outside-VRAM trigger's switches say.
    pub(super) fn pid_plan(&self, state: &SharedState, fill: Fill) -> Vec<Reading> {
        let mut current: Vec<Container> = state
            .runtime()
            .list()
            .iter()
            .filter(|e| e.state != RuntimeState::Starting && on_gpu(e))
            .map(container_of)
            .collect();
        current.extend(bench_container(state).ok().flatten());
        let plan = self.pids.lock().unwrap().plan(&current, fill);
        if let Some(done) = plan.done {
            self.read_pids(state, plan.inspect, done, false, true);
        }
        plan.wait
    }

    /// Run one batched `podman inspect` for `asked` in a task of its own, and
    /// record the answer before `done` is dropped (which is what every pass
    /// waiting on this read waits for).
    ///
    /// - Bounded by [`CONTROL_TIMEOUT`] (review finding 4): `podman inspect`
    ///   takes the container's lock, so it can queue behind a stop's grace
    ///   period or a pull's storage lock. A read that times out is recorded
    ///   as failed with that reason — the verdict is unavailable, today's
    ///   path — and the child is killed with the dropped future.
    /// - Recorded against the registry as it is when podman answered (review
    ///   finding 5, [`PidCache::record`](crate::vram::attribution::PidCache::record)).
    /// - `announce`: push a fresh `vram` frame once recorded (the view's
    ///   frame said "being read"). `walk`: also read the processes under
    ///   each init, so a tombstone made before any pass holds them
    ///   ([`Self::cache_pids`]).
    fn read_pids(
        &self,
        state: &SharedState,
        asked: Vec<Container>,
        done: attribution::ReadDone,
        announce: bool,
        walk: bool,
    ) {
        let registry = state.runtime();
        let cache = self.pids.clone();
        let tree = self.procs.read().unwrap().clone();
        let st = state.clone();
        tokio::spawn(async move {
            let names: Vec<String> = asked.iter().map(|c| c.name.clone()).collect();
            let answer =
                match tokio::time::timeout(CONTROL_TIMEOUT, registry.host_pids(&names)).await {
                    Ok(answer) => answer,
                    Err(_) => Err(format!(
                        "podman inspect did not answer within {}s (lmgw's timeout for control \
                     calls, vram::CONTROL_TIMEOUT)",
                        CONTROL_TIMEOUT.as_secs()
                    )),
                };
            let mut live: HashSet<u64> = registry
                .list()
                .iter()
                .filter(|e| on_gpu(e))
                .map(|e| e.generation)
                .collect();
            if let OnCard::Ready { generation } = bench_on_card(&st) {
                live.insert(generation);
            }
            let roots: Vec<(Container, u32)> = {
                let mut cache = cache.lock().unwrap();
                cache.record(&asked, &answer, &live);
                if walk {
                    asked
                        .iter()
                        .filter_map(|c| cache.root(c.generation).ok().map(|r| (c.clone(), r)))
                        .collect()
                } else {
                    Vec::new()
                }
            };
            drop(done);
            if announce {
                broadcast(&st);
            }
            if roots.is_empty() {
                return;
            }
            // A tree that cannot be read leaves that member's slot as it
            // is: its tombstone then holds the root alone.
            let members = tokio::task::spawn_blocking(move || {
                roots
                    .into_iter()
                    .filter_map(|(c, root)| {
                        Member::measure(c.generation, c.label, root, tree.as_ref())
                            .map_err(|e| tracing::warn!("vram: {e}"))
                            .ok()
                    })
                    .collect::<Vec<_>>()
            })
            .await;
            if let Ok(members) = members {
                cache.lock().unwrap().remember(&members);
            }
        });
    }
}

/// A ready image container whose charge is more than the driver shows of
/// it (`partial`): a partial start's, or one of a row whose weights all wait
/// off the card. What its processes hold is read beside the device read.
fn off_card_image(snap: &Snapshot, e: &RuntimeView) -> bool {
    e.class == Class::Image
        && e.state == RuntimeState::Ready
        && (e.grant.is_some()
            || snap
                .image_models
                .iter()
                .find(|m| m.model_id == e.model_id)
                .is_some_and(|m| super::plan::image::placement(&m.args).is_ok_and(|a| a.all_off())))
}

/// Where the benchmark run's container is ([`OnCard::Off`] with no run).
fn bench_on_card(state: &SharedState) -> OnCard {
    state.bench.current().map(|c| c.on_card).unwrap_or_default()
}

/// The benchmark run's container as the PID cache keys it, once it is loaded
/// (benchmark design §3.4): its processes are lmgw's share like a model
/// container's, and a tombstone once it is removed. `Err` while it loads —
/// its memory is still growing, as a `starting` entry's is
/// ([`unmeasured_start`]).
fn bench_container(state: &SharedState) -> Result<Option<Container>, String> {
    let Some(run) = state.bench.current() else {
        return Ok(None);
    };
    match (run.on_card, run.container) {
        (OnCard::Loading, _) => Err(format!(
            "benchmark run {} is loading chat/{} — its memory is not measured yet",
            run.run_id, run.model_id
        )),
        (OnCard::Ready { generation }, Some(name)) => Ok(Some(Container {
            generation,
            label: format!("chat/{} (benchmark run {})", run.model_id, run.run_id),
            name,
        })),
        _ => Ok(None),
    }
}

/// The registry's containers on the GPU (`on_cpu::on_gpu`) — what the
/// per-process attribution measures.
fn gpu_entries(state: &SharedState) -> Vec<RuntimeView> {
    state.runtime().list().into_iter().filter(on_gpu).collect()
}

/// A registry entry as the PID cache keys it.
fn container_of(e: &RuntimeView) -> Container {
    Container {
        generation: e.generation,
        label: format!("{}/{}", e.class.as_str(), e.model_id),
        name: e.container_name.clone(),
    }
}

/// lmgw's measured share, and the used memory outside it (§4.7): of every
/// card together ([`Measured::shares`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shares {
    pub lmgw: u64,
    pub outside: u64,
}

/// One card's part of a measurement (device-ledger design §2.3): what the
/// driver says is used there, and how much of it is lmgw's — `Err` with the
/// reason where the driver lists one of lmgw's processes there without a
/// figure ([`attribution::CardHeld`]). The outside use there is the
/// difference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CardShares {
    pub(super) id: DeviceId,
    pub(super) used: u64,
    /// The driver's free memory there, from the same device read.
    pub(super) free: u64,
    pub(super) lmgw: Result<u64, String>,
}

/// One measurement pass ([`VramScheduler::measure`]): per card, the used
/// memory and lmgw's share of it; per container, what its processes hold on
/// each card (device-ledger design §10, WP2); and the capacity and free
/// memory of the same device read, by the ledger's rule ([`pooled`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Measured {
    pub(super) cards: Vec<CardShares>,
    /// Each measured container's cards: the registry's and the benchmark
    /// run's, by generation.
    pub(super) members: Vec<attribution::MemberCards>,
    pub(super) total: u64,
    pub(super) raw_free: u64,
}

impl Measured {
    /// Every card's together, which is what the decisions read: lmgw's
    /// share summed over the cards, and the cards' used memory less it.
    /// `Err` with the first card's reason when a card's share is unknown:
    /// the sum would be a low guess, and is unknown as the pooled figure
    /// was.
    ///
    /// The outside use is the sum of the used memory less the sum of lmgw's
    /// share, not the sum of each card's outside use: a card on which the
    /// process reading saw more of lmgw's than the device reading did
    /// counts below zero there, as it did when the share was one pooled
    /// figure.
    pub(super) fn shares(&self) -> Result<Shares, String> {
        let mut lmgw: u64 = 0;
        let mut used: u64 = 0;
        for c in &self.cards {
            lmgw = lmgw.saturating_add(c.lmgw.clone()?);
            used = used.saturating_add(c.used);
        }
        Ok(Shares {
            lmgw,
            outside: used.saturating_sub(lmgw),
        })
    }
}

/// The cards of a measurement: every device, in the driver's order, with
/// lmgw's share there from `held` (each a part of it — a member's, a
/// tombstone's; one unknown part makes the card's share unknown, with the
/// first such part's reason). A card `held` names that the device read did
/// not list (one that left between the reads) comes last, with nothing
/// used: its share still counts, as it did in the pooled figure.
pub(super) fn card_shares(
    devices: &[GpuMemory],
    held: impl IntoIterator<Item = attribution::CardHeld>,
) -> Vec<CardShares> {
    let mut cards: Vec<CardShares> = devices
        .iter()
        .map(|d| CardShares {
            id: d.id.clone(),
            used: d.used_bytes,
            free: d.free_bytes,
            lmgw: Ok(0),
        })
        .collect();
    for h in held {
        let i = match cards.iter().position(|c| c.id == h.device) {
            Some(i) => i,
            None => {
                cards.push(CardShares {
                    id: h.device.clone(),
                    used: 0,
                    free: 0,
                    lmgw: Ok(0),
                });
                cards.len() - 1
            }
        };
        let card = &mut cards[i];
        card.lmgw = match (&card.lmgw, h.bytes) {
            (Ok(a), Ok(b)) => Ok(a.saturating_add(b)),
            (Err(e), _) => Err(e.clone()),
            (Ok(_), Err(e)) => Err(e),
        };
    }
    cards
}

/// One card in the ledger (device-ledger design §3.1, WP2): the driver's
/// figures, lmgw's share there and the outside use there where a
/// measurement was taken, and what is kept free there where that is known
/// per card. What the status surfaces show (`view.rs`); the decisions read
/// the same cards one by one through [`Ledger::capacity_on`], which places
/// each model on its cards (§5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CardCapacity {
    /// Its id, index and name, and the driver's total, used and free.
    pub(super) memory: GpuMemory,
    /// lmgw's share here, measured per process: `None` without a
    /// measurement (the pass's own reason says why), `Err` where the driver
    /// lists one of lmgw's processes here without a figure.
    pub(super) lmgw: Option<Result<u64, String>>,
    /// The used memory here less lmgw's share, never below zero; `None`
    /// exactly when the share is not known.
    pub(super) outside: Option<u64>,
    /// What the ledger keeps free here on top of the memory in use: the
    /// starts in flight, image peaks and pending loads of the models
    /// charged on this card alone (device-ledger design WP4). `None` where
    /// a model charged as a group on several cards (or on cards no device
    /// read named) keeps something free: how it divides among them is not
    /// known, and dividing it would be a guess; the ledger's `kept_free` is
    /// all cards' together. On a machine of one card it is all of that.
    pub(super) kept_free: Option<KeptFree>,
}

/// What the ledger keeps free on top of the memory in use, by the cards
/// each part of it is charged on (device-ledger design WP4): a start in
/// flight, an image peak or a pending load, each on its model's cards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct KeptOn(Vec<(Vec<DeviceId>, KeptFree)>);

impl KeptOn {
    fn add(&mut self, cards: &[DeviceId], part: KeptFree) {
        if part == KeptFree::default() {
            return;
        }
        self.0.push((cards.to_vec(), part));
    }

    fn starting(&mut self, cards: &[DeviceId], bytes: u64) {
        self.add(
            cards,
            KeptFree {
                starting_bytes: bytes,
                ..KeptFree::default()
            },
        );
    }

    /// A start in flight charged `charge` on `cards`, `bytes` in all and
    /// `margin` beside it: a split's part on each of its cards with the
    /// margin beside each (each card's process starts on its own, §4.3),
    /// anything else `bytes` and the margin on its cards.
    fn starting_charge(
        &mut self,
        charge: &CardCharge,
        cards: &[DeviceId],
        bytes: u64,
        margin: u64,
    ) {
        match charge.split_parts() {
            Some(parts) => {
                for (card, part) in parts {
                    let part = if bytes == 0 { 0 } else { part };
                    self.starting(std::slice::from_ref(&card), part.saturating_add(margin));
                }
            }
            None => self.starting(cards, bytes.saturating_add(margin)),
        }
    }

    fn image_peak(&mut self, cards: &[DeviceId], bytes: u64) {
        self.add(
            cards,
            KeptFree {
                image_peak_bytes: bytes,
                ..KeptFree::default()
            },
        );
    }

    fn pending_load(&mut self, cards: &[DeviceId], bytes: u64) {
        self.add(
            cards,
            KeptFree {
                pending_load_bytes: bytes,
                ..KeptFree::default()
            },
        );
    }

    /// What is kept free on `cards` at most (device-ledger design WP5): every
    /// part charged on one of them, a group's once however many of them it
    /// names, and every part on cards no device read named. A group on
    /// other cards too is counted whole: how it divides is not known, and
    /// the larger figure never reads room that is not there.
    pub(super) fn upper_on(&self, cards: &[DeviceId]) -> u64 {
        self.0
            .iter()
            .filter(|(on, _)| on.is_empty() || on.iter().any(|c| cards.contains(c)))
            .map(|(_, k)| {
                k.starting_bytes
                    .saturating_add(k.image_peak_bytes)
                    .saturating_add(k.pending_load_bytes)
            })
            .fold(0, u64::saturating_add)
    }

    /// What is kept free on `card` alone: the parts charged on it alone.
    /// `None` where a part on several cards, or on cards no device read
    /// named, could be on it.
    pub(super) fn on(&self, card: &DeviceId) -> Option<KeptFree> {
        let mut k = KeptFree::default();
        for (cards, part) in &self.0 {
            match cards.as_slice() {
                [one] if one == card => {
                    k.starting_bytes = k.starting_bytes.saturating_add(part.starting_bytes);
                    k.image_peak_bytes = k.image_peak_bytes.saturating_add(part.image_peak_bytes);
                    k.pending_load_bytes =
                        k.pending_load_bytes.saturating_add(part.pending_load_bytes);
                }
                [_] => {}
                cards if cards.is_empty() || cards.contains(card) => return None,
                _ => {}
            }
        }
        Some(k)
    }
}

/// The cards a registry entry is charged on, as one ledger read finds them
/// ([`VramScheduler::entry_cards`]).
pub(super) struct EntryCards {
    /// Its cards, as a charge whose bytes are the caller's to set
    /// ([`CardCharge::with_total`]): empty where every card it was recorded
    /// on is gone.
    pub(super) charge: CardCharge,
    /// What the resident says of them: a recorded card that is gone, or a
    /// container adopted without a record of its cards.
    pub(super) note: Option<String>,
    /// Every card it was recorded on is gone.
    nowhere: bool,
}

impl EntryCards {
    /// The note of an entry charged nowhere, when it is.
    fn nowhere(&self) -> Option<String> {
        self.nowhere.then(|| self.note.clone().unwrap_or_default())
    }
}

impl VramScheduler {
    /// The cards entry `e` is charged on, of `devices` (this read's;
    /// device-ledger design §3.2, §3.3): the ones admission charged its
    /// start on or its label recorded ([`RuntimeView::card_charge`]), else
    /// the cards its row's arguments pass ([`super::charge::seen`]). A
    /// recorded card this read does not list is gone: the entry is charged
    /// on the others, nowhere where none is left, and the resident says so
    /// (logged once per container). With no device read nothing is gone.
    pub(super) fn entry_cards(
        &self,
        snap: &Snapshot,
        e: &RuntimeView,
        devices: &[GpuMemory],
    ) -> EntryCards {
        let Some(recorded) = &e.card_charge else {
            let cards = match crate::runtime::descriptor::model_runtime(snap, e.class, &e.model_id)
            {
                Some(rt) => super::charge::seen(
                    &rt.extra_run_args,
                    e.placement,
                    self.engine_build(e.class).as_deref(),
                    devices,
                ),
                None => devices.iter().map(|d| d.id.clone()).collect(),
            };
            let note = (e.adopted && cards.len() > 1).then(|| {
                format!(
                    "Adopted from a container that does not record its cards: charged its \
                     estimate on its {} cards together, not what it holds on each, until it is \
                     started again",
                    cards.len()
                )
            });
            return EntryCards {
                charge: CardCharge::over(cards, 0),
                note,
                nowhere: false,
            };
        };
        if devices.is_empty() {
            return EntryCards {
                charge: recorded.clone(),
                note: None,
                nowhere: false,
            };
        }
        let present: Vec<DeviceId> = devices.iter().map(|d| d.id.clone()).collect();
        let (charge, gone) = recorded.on_present(&present);
        if gone.is_empty() {
            return EntryCards {
                charge,
                note: None,
                nowhere: false,
            };
        }
        let nowhere = charge.is_empty();
        let note = format!(
            "Started on {} {}, which this machine no longer lists: charged {}",
            if gone.len() == 1 { "card" } else { "cards" },
            CardCharge::names(&gone),
            if nowhere {
                "nowhere"
            } else {
                "on its other cards"
            }
        );
        if self.cards_gone_said.lock().unwrap().insert(e.generation) {
            tracing::warn!(
                "{}/{}: {note}; the engine runs on whatever it has",
                e.class.as_str(),
                e.model_id
            );
        }
        EntryCards {
            charge,
            note: Some(note),
            nowhere,
        }
    }
}

impl Ledger {
    /// The capacity of `cards` alone (device-ledger design §5, WP5): their
    /// totals and the driver's free memory on them, less what the ledger
    /// keeps free there ([`KeptOn::upper_on`]) and what the claims of other
    /// starts take there ([`super::scheduler::Claim::kept`]): every claim
    /// for a start without one of its own (`own: None`), the claims made
    /// before its own for one that has (§5.5, served in order). Under a
    /// declared budget never more than the budget leaves on every card
    /// together (§9.2). The pooled capacity where `cards` is empty or no
    /// device read named any (a declared budget without telemetry); `None`
    /// without a capacity.
    pub(super) fn capacity_on(&self, cards: &[DeviceId], own: Option<u64>) -> Option<Capacity> {
        let pooled = self.capacity.as_ref()?;
        let on: Vec<&GpuMemory> = self
            .devices
            .iter()
            .filter(|d| cards.contains(&d.id))
            .collect();
        if on.is_empty() {
            return Some(pooled.clone());
        }
        let ids: Vec<DeviceId> = on.iter().map(|d| d.id.clone()).collect();
        let claimed = self.claimed_on(&ids, own);
        let total: u64 = on.iter().map(|d| d.total_bytes).sum();
        let raw: u64 = on.iter().map(|d| d.free_bytes).sum();
        let kept = self.kept_on.upper_on(&ids);
        let mut free = raw.saturating_sub(kept).saturating_sub(claimed);
        let mut total = total;
        if self.budget > 0 {
            total = total.min(self.budget);
            free = free.min(pooled.free.saturating_sub(self.claimed_on(&[], own)));
        }
        Some(Capacity {
            total,
            free,
            measured: pooled.measured,
            kept_free: pooled.kept_free,
        })
    }

    /// What the claims other than `own` (and, for a claim, those made after
    /// it) take on `cards`; on every card for `cards` empty.
    fn claimed_on(&self, cards: &[DeviceId], own: Option<u64>) -> u64 {
        self.claims
            .iter()
            .filter(|c| match own {
                Some(id) => c.id < id,
                None => true,
            })
            .filter(|c| cards.is_empty() || c.cards.iter().any(|x| cards.contains(x)))
            .map(|c| c.kept(&self.residents))
            .fold(0, u64::saturating_add)
    }

    /// What lmgw's resident `r` holds on `card` at most: its part there, a
    /// group's whole where the group names the card (how a group divides is
    /// not known), the peak kept free for it, and the first-prompt margin
    /// kept free for it there ([`ResidentView::kept_bytes`], where it is
    /// charged on that card alone or on no card named): what stopping it
    /// frees there (owner, 2026-11-18); 0 where it is not there.
    pub(super) fn held_on(r: &ResidentView, card: &DeviceId) -> u64 {
        let on = match r.charge.alone_on(card) {
            Some(b) => b,
            None => r
                .charge
                .parts()
                .iter()
                .filter(|p| p.cards.is_empty() || p.cards.contains(card))
                .map(|p| p.bytes)
                .fold(0, u64::saturating_add),
        };
        if on == 0 {
            return 0;
        }
        let kept = match r.charge.cards().as_slice() {
            [] => r.kept_bytes,
            [one] if one == card => r.kept_bytes,
            _ => 0,
        };
        on.saturating_add(r.peak_extra_bytes.unwrap_or(0))
            .saturating_add(kept)
    }

    /// Whether resident `r` is on one of `cards` (every resident is, for
    /// `cards` empty): what an eviction or a wait on those cards may free.
    pub(super) fn is_on(r: &ResidentView, cards: &[DeviceId]) -> bool {
        cards.is_empty()
            || r.charge
                .parts()
                .iter()
                .any(|p| p.cards.is_empty() || p.cards.iter().any(|c| cards.contains(c)))
    }

    /// Under a declared budget over several cards, what the budget leaves
    /// for a start (`own`: its claim) beside everything in use on every card
    /// and other starts' claims: what a split's parts together must fit
    /// (owner, 2026-11-18, `place::Card::pool`). `None` without a budget and
    /// on one card.
    pub(super) fn budget_left(&self, own: Option<u64>) -> Option<u64> {
        if self.budget == 0 || self.devices.len() < 2 {
            return None;
        }
        let pooled = self.capacity.as_ref()?;
        Some(pooled.free.saturating_sub(self.claimed_on(&[], own)))
    }

    /// The declared budget, on a machine of several cards: what a split's
    /// parts together may take at most, with nothing else of lmgw's there.
    pub(super) fn budget_total(&self) -> Option<u64> {
        (self.budget > 0 && self.devices.len() > 1).then_some(self.budget)
    }

    /// What is kept free on `card` alone ([`CardCapacity::kept_free`]);
    /// `None` without a capacity.
    pub(super) fn kept_free_on(&self, card: &DeviceId) -> Option<KeptFree> {
        self.capacity.as_ref()?;
        self.kept_on.on(card)
    }

    /// Every card the device read listed, in the driver's order, with
    /// `measured`'s shares where a measurement was taken. A card the
    /// measurement's own device read did not list has no share.
    pub(super) fn cards(&self, measured: Option<&Measured>) -> Vec<CardCapacity> {
        self.devices
            .iter()
            .map(|d| {
                let shares = measured.and_then(|m| m.cards.iter().find(|c| c.id == d.id));
                let lmgw = shares.map(|c| c.lmgw.clone());
                let outside = shares
                    .and_then(|c| c.lmgw.as_ref().ok().map(|own| c.used.saturating_sub(*own)));
                CardCapacity {
                    memory: d.clone(),
                    lmgw,
                    outside,
                    kept_free: self.kept_free_on(&d.id),
                }
            })
            .collect()
    }
}

/// The pooled capacity and measured free memory of `devices`, by the
/// ledger's rule: a declared budget is the capacity and what "free" is
/// measured against, otherwise the devices' own figures. One definition for
/// the ledger's `raw_free` and the verdict's, so the two cannot drift.
pub(super) fn pooled(budget: u64, devices: &[GpuMemory]) -> (u64, u64) {
    if budget == 0 {
        (
            devices.iter().map(|d| d.total_bytes).sum(),
            devices.iter().map(|d| d.free_bytes).sum(),
        )
    } else {
        let used: u64 = devices.iter().map(|d| d.used_bytes).sum();
        (budget, budget.saturating_sub(used))
    }
}

/// Two readings of the driver's process lists as one, per device and PID
/// the larger figure (review finding 2): the device read between them may
/// have seen either, and the larger one is the direction that never reads
/// lmgw's memory as outside use. A PID only one reading lists on a device
/// keeps that reading's figure there. A figure one reading could not give
/// makes the PID's unknown on that device — never the other reading's,
/// which may be the smaller. The devices in the first reading's order, then
/// any only the second lists.
pub(super) fn larger_per_pid(
    first: &[DeviceProcesses],
    second: &[DeviceProcesses],
) -> Vec<DeviceProcesses> {
    let mut order: Vec<&DeviceId> = Vec::new();
    let mut per: std::collections::HashMap<
        &DeviceId,
        std::collections::BTreeMap<u32, Option<u64>>,
    > = Default::default();
    for d in first.iter().chain(second) {
        if !per.contains_key(&d.device) {
            order.push(&d.device);
        }
        let on = per.entry(&d.device).or_default();
        for p in &d.processes {
            on.entry(p.pid)
                .and_modify(|b| {
                    *b = match (*b, p.bytes) {
                        (Some(a), Some(c)) => Some(a.max(c)),
                        _ => None,
                    }
                })
                .or_insert(p.bytes);
        }
    }
    order
        .into_iter()
        .map(|device| DeviceProcesses {
            device: device.clone(),
            processes: per
                .remove(device)
                .unwrap_or_default()
                .into_iter()
                .map(|(pid, bytes)| ProcessMemory { pid, bytes })
                .collect(),
        })
        .collect()
}

/// Which lmgw containers exist and in what state, and which starts are
/// reserved: what a measurement compares before and after its reads (review
/// finding 2). The in-flight count and the idle age are left out — they
/// change nothing on the card.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Roster {
    entries: Vec<(u64, RuntimeState)>,
    reservations: Vec<u64>,
    /// The benchmark run's container, which is no registry entry.
    bench: OnCard,
}

impl Roster {
    pub(super) fn of(entries: &[RuntimeView], reservations: &[Reservation]) -> Self {
        let mut e: Vec<(u64, RuntimeState)> =
            entries.iter().map(|e| (e.generation, e.state)).collect();
        e.sort_by_key(|(g, _)| *g);
        let mut r: Vec<u64> = reservations.iter().map(|r| r.id).collect();
        r.sort_unstable();
        Self {
            entries: e,
            reservations: r,
            bench: OnCard::Off,
        }
    }

    /// With the benchmark run's container.
    pub(super) fn and_bench(mut self, bench: OnCard) -> Self {
        self.bench = bench;
        self
    }
}

/// Why the outside-VRAM trigger is switched off by configuration, if it is —
/// the checks that cost nothing, so a request (or a frame) behind any of them
/// probes nothing and spawns nothing (candidate-aliases design §7 item 14).
pub(super) fn trigger_off(snap: &Snapshot) -> Option<String> {
    let s = &snap.settings;
    if !s.vram.fallback_on_external {
        return Some(format!(
            "falling back when VRAM outside lmgw's control is short is off ({}), so only the GPU \
             hold sends a local request to its fallback.",
            crate::config::FALLBACK_ON_EXTERNAL_LABEL
        ));
    }
    if !s.vram.enabled {
        return Some(format!(
            "GPU memory arbitration is off ({}), so admission does not arbitrate and there is no \
             shortfall to measure.",
            crate::config::ADMISSION_LABEL
        ));
    }
    if s.hold.active {
        return Some(
            "while the hold is on, every local request goes to its own fallback or is refused."
                .into(),
        );
    }
    // A benchmark run's lease does not switch it off (benchmark design
    // §3.4): the run's container is attributed as lmgw's
    // ([`bench_container`]), so outside use stays measured during a run.
    None
}

impl VramScheduler {
    /// Why the verdict must wait for boot, while it must (see
    /// [`Self::set_boot_settled`]).
    pub(super) fn boot_unsettled(&self) -> Option<String> {
        (!self.boot_settled()).then(|| {
            "boot reconciliation still running — containers a previous lmgw left on the card \
             are not adopted yet, so their memory would read as outside use"
                .to_string()
        })
    }
}

/// A start whose memory the driver cannot show yet, if there is one: a
/// registry entry still `starting`, or a reservation (a start admitted but
/// not in the registry yet). Either would make lmgw's measured share a low
/// guess, so either makes the trigger unavailable (§4.7).
pub(super) fn unmeasured_start(
    entries: &[RuntimeView],
    reservations: &[Reservation],
) -> Option<String> {
    if let Some(e) = entries.iter().find(|e| e.state == RuntimeState::Starting) {
        return Some(format!(
            "{}/{} is starting — its memory is not measured yet",
            e.class.as_str(),
            e.model_id
        ));
    }
    reservations
        .iter()
        .find(|r| {
            !entries
                .iter()
                .any(|e| e.class == r.class && e.model_id == r.model_id)
        })
        .map(|r| {
            format!(
                "{}/{} has just been admitted and its container is not measured yet",
                r.class.as_str(),
                r.model_id
            )
        })
}

#[cfg(test)]
mod kept_on_tests {
    use super::*;

    fn id(i: u32) -> DeviceId {
        DeviceId::new(format!("GPU-{i}"))
    }

    /// What is kept free on a card alone: the parts charged on it alone,
    /// summed; unknown where a group or a part of unnamed cards is on it;
    /// nothing kept is zero, not unknown.
    #[test]
    fn what_is_kept_free_on_a_card_alone() {
        let mut k = KeptOn::default();
        assert_eq!(k.on(&id(0)), Some(KeptFree::default()));
        k.starting(&[id(0)], 3);
        k.image_peak(&[id(0)], 4);
        k.pending_load(&[id(1)], 5);
        k.starting(&[id(1), id(2)], 6);
        // Nothing kept is no part at all, even of a group.
        k.starting(&[id(3), id(0)], 0);
        assert_eq!(
            k.on(&id(0)),
            Some(KeptFree {
                starting_bytes: 3,
                image_peak_bytes: 4,
                pending_load_bytes: 0,
            })
        );
        assert_eq!(k.on(&id(1)), None);
        assert_eq!(k.on(&id(2)), None);
        assert_eq!(k.on(&id(3)), Some(KeptFree::default()));
        k.pending_load(&[], 1);
        assert_eq!(k.on(&id(3)), None);
    }
}
