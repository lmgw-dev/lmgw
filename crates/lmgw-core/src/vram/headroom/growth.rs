//! The record of outside use auto headroom learns from, per card
//! ([`super`], migrations 0106 and 0109; device-ledger design §9.1, WP13):
//! per hour the lowest and the highest reading. The floor is capped at the
//! highest of the window ([`Growth::most`]), the learned room is that less
//! the newest reading, and the lowest is what other programs hold at rest.
//! The hour's largest rise (`rise_bytes`) is still recorded, and no longer
//! read.

use std::collections::BTreeMap;

use crate::state::SharedState;
use crate::store::OutsideHour;
use crate::vram::nvml::{DeviceId, GpuMemory};

use super::{SAMPLE_INTERVAL, WINDOW_DAYS};

/// The current hour, Unix time / 3600.
pub(crate) fn now_hour() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs() / 3600).unwrap_or(i64::MAX))
}

fn window_hours() -> i64 {
    i64::try_from(WINDOW_DAYS * 24).unwrap_or(i64::MAX)
}

/// One card's hours of readings in memory, oldest first.
#[derive(Debug, Default, Clone)]
pub struct Growth {
    hours: Vec<OutsideHour>,
    /// The newest reading this process took on the card, in bytes.
    pub(super) last: Option<u64>,
}

impl Growth {
    /// The hours in the window ending at `now`.
    fn window(&self, now: i64) -> impl Iterator<Item = &OutsideHour> {
        let since = now.saturating_sub(window_hours() - 1);
        self.hours.iter().filter(move |h| h.hour >= since)
    }

    /// Take `outside` in as a reading of hour `hour`: the hour after it when
    /// the reading changed it (a new hour, a new lowest, highest or rise),
    /// `None` when the hour stands as it was, so a reading every second
    /// writes to the store only when there is something new to keep.
    pub(crate) fn record(&mut self, hour: i64, outside: u64) -> Option<OutsideHour> {
        self.last = Some(outside);
        let since = hour.saturating_sub(window_hours() - 1);
        self.hours.retain(|h| h.hour >= since);
        if let Some(h) = self.hours.iter_mut().find(|h| h.hour == hour) {
            let before = *h;
            h.rise_bytes = h.rise_bytes.max(outside.saturating_sub(h.low_bytes));
            h.low_bytes = h.low_bytes.min(outside);
            h.high_bytes = h.high_bytes.max(outside);
            return (*h != before).then_some(*h);
        }
        let h = OutsideHour {
            hour,
            low_bytes: outside,
            high_bytes: outside,
            rise_bytes: 0,
        };
        self.hours.push(h);
        self.hours.sort_by_key(|h| h.hour);
        Some(h)
    }

    /// A stored hour, unless one of that hour is in memory already.
    fn load(&mut self, s: OutsideHour) {
        if !self.hours.iter().any(|h| h.hour == s.hour) {
            self.hours.push(s);
            self.hours.sort_by_key(|h| h.hour);
        }
    }

    /// The most other programs held in the window ending at `now`, and the
    /// first hour they held that much. `None` without readings.
    pub(crate) fn most(&self, now: i64) -> Option<(u64, i64)> {
        self.window(now)
            .fold(None, |best: Option<(u64, i64)>, h| match best {
                Some((b, _)) if b >= h.high_bytes => best,
                _ => Some((h.high_bytes, h.hour)),
            })
    }

    /// The separate times in the window ending at `now` that other programs
    /// held more than `level`: runs of hours in a row whose highest reading
    /// is above it, an hour without readings or at most `level` between
    /// two runs.
    pub(crate) fn times_above(&self, now: i64, level: u64) -> u64 {
        let mut times = 0;
        let mut last: Option<i64> = None;
        for h in self.window(now).filter(|h| h.high_bytes > level) {
            if last != Some(h.hour - 1) {
                times += 1;
            }
            last = Some(h.hour);
        }
        times
    }

    /// Other programs' use at rest: the lowest reading in the window ending
    /// at `now` — what is on the card whatever lmgw does, which a model can
    /// never have. `None` without readings.
    pub(crate) fn at_rest(&self, now: i64) -> Option<u64> {
        self.window(now).map(|h| h.low_bytes).min()
    }

    /// The newest reading this process took, else the reading at rest.
    pub(crate) fn recent(&self, now: i64) -> Option<u64> {
        self.last.or_else(|| self.at_rest(now))
    }

    /// Hours of the window ending at `now` with readings.
    pub(crate) fn hours(&self, now: i64) -> u64 {
        self.window(now).count() as u64
    }
}

/// A card the headroom is kept on ([`Record::counted`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Counted {
    /// `None`: no card is known yet, neither from a device read nor from a
    /// reading, and the headroom is one fixed figure, as before cards.
    pub id: Option<DeviceId>,
    pub index: u32,
    pub name: String,
}

/// The record of every card ([`super`]): each card's hours, and the cards
/// the last device read listed.
#[derive(Debug, Default)]
pub struct Record {
    cards: BTreeMap<DeviceId, Growth>,
    /// The cards the last device read listed, in the driver's order
    /// (`VramScheduler::ledger`): the cards the headroom is kept on.
    present: Vec<GpuMemory>,
    /// The persisted hours were read in.
    pub(super) loaded: bool,
}

impl Record {
    /// Card `device`'s readings, if it has any.
    pub(crate) fn card(&self, device: &DeviceId) -> Option<&Growth> {
        self.cards.get(device)
    }

    /// Take `outside` in as a reading of card `device` in hour `hour`; the
    /// card's hour after it, when the reading changed it ([`Growth::record`]).
    pub(crate) fn record(
        &mut self,
        device: &DeviceId,
        hour: i64,
        outside: u64,
    ) -> Option<OutsideHour> {
        self.cards
            .entry(device.clone())
            .or_default()
            .record(hour, outside)
    }

    /// Hours read back from the store, merged under what is in memory.
    pub(crate) fn load(&mut self, stored: Vec<(DeviceId, OutsideHour)>) {
        for (device, h) in stored {
            self.cards.entry(device).or_default().load(h);
        }
        self.loaded = true;
    }

    /// Forget card `device`'s readings, or every card's with `None` (the
    /// Forget learned growth action, device-ledger design ruling 10):
    /// auto learns again from the next reading, with the uncapped floor
    /// meanwhile.
    pub(crate) fn forget(&mut self, device: Option<&DeviceId>) {
        match device {
            Some(d) => {
                self.cards.remove(d);
            }
            None => self.cards.clear(),
        }
    }

    /// The cards a device read listed, in the driver's order.
    pub(crate) fn set_present(&mut self, devices: &[GpuMemory]) {
        if self.present.len() != devices.len()
            || self
                .present
                .iter()
                .zip(devices)
                .any(|(a, b)| a.id != b.id || a.index != b.index || a.name != b.name)
        {
            self.present = devices.to_vec();
        }
    }

    /// The cards the headroom is kept on: the ones the last device read
    /// listed; before any did, the ones with readings; with neither, one
    /// card nothing is known of (`id` `None`), on which the fixed figure is
    /// kept once, as before cards.
    pub(crate) fn counted(&self) -> Vec<Counted> {
        if !self.present.is_empty() {
            return self
                .present
                .iter()
                .map(|d| Counted {
                    id: Some(d.id.clone()),
                    index: d.index,
                    name: d.name.clone(),
                })
                .collect();
        }
        if !self.cards.is_empty() {
            return self
                .cards
                .keys()
                .enumerate()
                .map(|(i, id)| Counted {
                    id: Some(id.clone()),
                    index: u32::try_from(i).unwrap_or(u32::MAX),
                    name: String::new(),
                })
                .collect();
        }
        vec![Counted {
            id: None,
            index: 0,
            name: String::new(),
        }]
    }

    /// Every card with readings that the last device read did not list.
    pub(crate) fn absent(&self) -> Vec<DeviceId> {
        self.cards
            .keys()
            .filter(|id| !self.present.iter().any(|d| &d.id == *id))
            .cloned()
            .collect()
    }
}

/// The sampler (module doc of [`super`]): every [`SAMPLE_INTERVAL`], one
/// reading of outside use per card where the attribution can take one —
/// admission on, boot settled, per-process figures, no start in flight —
/// recorded and written to the card's hour.
pub async fn run(st: SharedState) {
    let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        sample(&st).await;
    }
}

/// One pass of [`run`]; the readings it recorded, summed over the cards, if
/// it took any. A card whose share of lmgw's is unknown (the driver lists
/// one of lmgw's processes there without a figure) is not read this pass;
/// the others are (device-ledger design §2.3).
pub async fn sample(st: &SharedState) -> Option<u64> {
    let now = now_hour();
    let since = now.saturating_sub(window_hours() - 1);
    let loaded = st.vram.growth.lock().unwrap().loaded;
    if !loaded {
        match crate::store::list_vram_outside_hours(&st.db, since).await {
            Ok(stored) => st.vram.growth.lock().unwrap().load(stored),
            Err(e) => {
                tracing::debug!("VRAM headroom: the outside-use record could not be read: {e}");
                return None;
            }
        }
    }
    let snap = st.snapshot();
    if !snap.settings.vram.enabled || !st.vram.boot_settled() {
        return None;
    }
    // Not while the hold or a benchmark keeps lmgw's models off the card:
    // whatever has the card then — the job the owner held it for, a
    // benchmark's own engine — is no growth of the desktop's, and would
    // read as one (per-model containers §4, "Headroom").
    if snap.gpu_block().is_some() {
        tracing::trace!("VRAM headroom: no reading while the card is held");
        return None;
    }
    // The engine's cost on a card, read once per container as soon as it
    // is ready and idle (device-ledger design §4.2): a start's own first
    // request is often still in flight when the start settles, and the
    // reading then waited for the next start (owner, 2026-11-18).
    st.vram.read_engine_costs(st);
    let l = st.vram.ledger(st, &snap).await;
    let measured = match st
        .vram
        .measure(st, &snap, &l, crate::vram::attribution::Fill::View)
        .await
    {
        Ok(m) => m,
        Err(why) => {
            tracing::trace!("VRAM headroom: no reading this pass: {why}");
            return None;
        }
    };
    let mut read: Option<u64> = None;
    for card in &measured.cards {
        // A card the ledger's device read did not list left between the
        // two reads: its used memory is not known.
        if !l.devices.iter().any(|d| d.id == card.id) {
            continue;
        }
        let own = match &card.lmgw {
            Ok(own) => *own,
            Err(why) => {
                tracing::trace!("VRAM headroom: no reading of {} this pass: {why}", card.id);
                continue;
            }
        };
        let outside = card.used.saturating_sub(own);
        // Written only when the reading changed the card's hour: a new
        // hour, or a new lowest, highest or rise in it.
        if let Some(hour) = st.vram.note_outside(&card.id, outside) {
            if let Err(e) = crate::store::put_vram_outside_hour(&st.db, &card.id, hour, since).await
            {
                tracing::debug!("VRAM headroom: the outside-use reading was not written: {e}");
            }
        }
        read = Some(read.unwrap_or(0).saturating_add(outside));
    }
    read
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    /// A reading every second writes only what changed the hour: a new
    /// hour, a new highest, a new lowest or a larger rise; the same figure
    /// again, or one between the hour's lowest and highest that rises no
    /// further, is nothing to store. The newest reading is still taken in.
    #[test]
    fn a_reading_that_changes_nothing_is_nothing_to_store() {
        let mut g = Growth::default();
        let first = g.record(100, 200 * MIB).expect("a new hour");
        assert_eq!((first.low_bytes, first.high_bytes), (200 * MIB, 200 * MIB));
        assert_eq!(g.record(100, 200 * MIB), None, "the same figure");
        let up = g.record(100, 500 * MIB).expect("a new highest");
        assert_eq!((up.high_bytes, up.rise_bytes), (500 * MIB, 300 * MIB));
        assert_eq!(g.record(100, 300 * MIB), None, "between lowest and highest");
        assert_eq!(g.recent(100), Some(300 * MIB), "the newest is still taken");
        let down = g.record(100, 100 * MIB).expect("a new lowest");
        assert_eq!(down.low_bytes, 100 * MIB);
        let rise = g
            .record(100, 500 * MIB)
            .expect("back at the highest: a larger rise");
        assert_eq!(rise.rise_bytes, 400 * MIB);
        assert_eq!(g.record(100, 500 * MIB), None, "the highest again");
        assert!(g.record(101, 300 * MIB).is_some(), "a new hour");
    }

    /// The most is the highest reading of the window, at the first hour it
    /// was reached, and leaves the window with its hour: 130 MiB at rest, a
    /// browser to 400 within the first hour, back down, a game to 2 GiB two
    /// hours later.
    #[test]
    fn the_most_is_the_windows_highest_reading() {
        let mut g = Growth::default();
        assert_eq!(g.most(100), None);
        g.record(100, 130 * MIB);
        g.record(100, 400 * MIB);
        g.record(100, 200 * MIB);
        assert_eq!(g.most(100), Some((400 * MIB, 100)));
        g.record(101, 150 * MIB);
        g.record(102, 2048 * MIB);
        assert_eq!(g.most(102), Some((2048 * MIB, 102)));
        assert_eq!(g.hours(102), 3);
        assert_eq!(g.at_rest(102), Some(130 * MIB));
        assert_eq!(g.recent(102), Some(2048 * MIB), "the newest reading");
        // Out of the window, the old hours no longer count.
        let later = 102 + window_hours();
        g.record(later, 300 * MIB);
        assert_eq!(g.most(later), Some((300 * MIB, later)));
        assert_eq!(g.at_rest(later), Some(300 * MIB));
        let mut r = Record::default();
        let (a, b) = (DeviceId::new("GPU-a"), DeviceId::new("GPU-b"));
        r.record(&a, later, MIB);
        r.record(&b, later, MIB);
        r.forget(Some(&a));
        assert!(r.card(&a).is_none() && r.card(&b).is_some(), "only a's");
        r.forget(None);
        assert!(r.card(&b).is_none(), "every card's");
    }

    /// A run of hours above the level is one time; an hour at or below it,
    /// or without readings, ends a run.
    #[test]
    fn the_times_above_a_level_count_runs_of_hours() {
        let mut g = Growth::default();
        for (hour, mib) in [
            (10, 100),
            (11, 900),
            (12, 950),
            (13, 100),
            (14, 800),
            (16, 700),
            (17, 500),
        ] {
            g.record(hour, mib * MIB);
        }
        assert_eq!(g.times_above(17, 500 * MIB), 3, "11-12, 14, 16");
        assert_eq!(g.times_above(17, 900 * MIB), 1);
        assert_eq!(g.times_above(17, 950 * MIB), 0);
    }
}
