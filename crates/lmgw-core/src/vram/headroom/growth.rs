//! The record of outside use auto headroom learns from
//! ([`super`], migration 0106).

use crate::state::SharedState;
use crate::store::OutsideHour;

use super::{HOURS_NEEDED, SAMPLE_INTERVAL, WINDOW_DAYS};

/// The current hour, Unix time / 3600.
pub(crate) fn now_hour() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs() / 3600).unwrap_or(i64::MAX))
}

fn window_hours() -> i64 {
    i64::try_from(WINDOW_DAYS * 24).unwrap_or(i64::MAX)
}

/// The hours of readings in memory, oldest first.
#[derive(Debug, Default)]
pub struct Growth {
    hours: Vec<OutsideHour>,
    /// The newest reading this process took, in bytes.
    pub(super) last: Option<u64>,
    /// The persisted hours were read in.
    pub(super) loaded: bool,
}

impl Growth {
    /// The hours in the window ending at `now`.
    fn window(&self, now: i64) -> impl Iterator<Item = &OutsideHour> {
        let since = now.saturating_sub(window_hours() - 1);
        self.hours.iter().filter(move |h| h.hour >= since)
    }

    /// Take `outside` in as a reading of hour `hour`; the hour after it.
    pub(crate) fn record(&mut self, hour: i64, outside: u64) -> OutsideHour {
        self.last = Some(outside);
        let since = hour.saturating_sub(window_hours() - 1);
        self.hours.retain(|h| h.hour >= since);
        if let Some(h) = self.hours.iter_mut().find(|h| h.hour == hour) {
            h.rise_bytes = h.rise_bytes.max(outside.saturating_sub(h.low_bytes));
            h.low_bytes = h.low_bytes.min(outside);
            h.high_bytes = h.high_bytes.max(outside);
            return *h;
        }
        let h = OutsideHour {
            hour,
            low_bytes: outside,
            high_bytes: outside,
            rise_bytes: 0,
        };
        self.hours.push(h);
        self.hours.sort_by_key(|h| h.hour);
        h
    }

    /// Hours read back from the store, merged under what is in memory.
    pub(crate) fn load(&mut self, stored: Vec<OutsideHour>) {
        for s in stored {
            if !self.hours.iter().any(|h| h.hour == s.hour) {
                self.hours.push(s);
            }
        }
        self.hours.sort_by_key(|h| h.hour);
        self.loaded = true;
    }

    /// The largest rise of outside use in the window ending at `now`: the
    /// most a reading exceeded any earlier one. `None` without readings.
    pub(crate) fn rise(&self, now: i64) -> Option<u64> {
        self.largest(now).map(|(rise, _)| rise)
    }

    /// [`Self::rise`], and the hour whose reading rose that far: the first
    /// one, where several did.
    pub(crate) fn largest(&self, now: i64) -> Option<(u64, i64)> {
        let mut low: Option<u64> = None;
        let mut best: Option<(u64, i64)> = None;
        for h in self.window(now) {
            let across = low.map_or(0, |l| h.high_bytes.saturating_sub(l));
            let here = h.rise_bytes.max(across);
            if best.is_none_or(|(b, _)| here > b) {
                best = Some((here, h.hour));
            }
            low = Some(low.map_or(h.low_bytes, |l| l.min(h.low_bytes)));
        }
        best
    }

    /// Other programs' use at rest: the lowest reading in the window ending
    /// at `now` — what is on the card whatever lmgw does, which a model can
    /// never have. `None` without readings.
    pub(crate) fn at_rest(&self, now: i64) -> Option<u64> {
        self.window(now).map(|h| h.low_bytes).min()
    }

    /// Forget every reading (the owner's "forget learned growth"): auto
    /// learns again from now on, with the fixed value meanwhile.
    pub(crate) fn forget(&mut self) {
        self.hours.clear();
        self.last = None;
    }

    /// Hours of the window ending at `now` with readings.
    pub(crate) fn hours(&self, now: i64) -> u64 {
        self.window(now).count() as u64
    }

    /// The learned growth, once [`HOURS_NEEDED`] hours have readings.
    pub(crate) fn learned(&self, now: i64) -> Option<u64> {
        (self.hours(now) >= HOURS_NEEDED)
            .then(|| self.rise(now))
            .flatten()
    }
}

/// The sampler (module doc of [`super`]): every [`SAMPLE_INTERVAL`], one
/// reading of outside use where the attribution can take one — admission
/// on, boot settled, per-process figures, no start in flight — recorded and
/// written to its hour.
pub async fn run(st: SharedState) {
    let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        sample(&st).await;
    }
}

/// One pass of [`run`]; the reading it recorded, if it took one.
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
    let l = st.vram.ledger(st, &snap).await;
    let measured = st
        .vram
        .measure(st, &snap, &l, crate::vram::attribution::Fill::View)
        .await;
    let outside = match measured {
        Ok(m) => m.shares.outside,
        Err(why) => {
            tracing::trace!("VRAM headroom: no reading this pass: {why}");
            return None;
        }
    };
    let hour = st.vram.note_outside(outside);
    if let Err(e) = crate::store::put_vram_outside_hour(&st.db, hour, since).await {
        tracing::debug!("VRAM headroom: the outside-use reading was not written: {e}");
    }
    Some(outside)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    /// The largest rise counts across hours as well as within one, and a
    /// fall does not: 130 MiB at rest, a browser to 400 within the first
    /// hour, back down, a game to 2 GiB two hours later.
    #[test]
    fn the_largest_rise_spans_hours() {
        let mut g = Growth::default();
        g.record(100, 130 * MIB);
        g.record(100, 400 * MIB);
        g.record(100, 200 * MIB);
        assert_eq!(g.rise(100), Some(270 * MIB));
        g.record(101, 150 * MIB);
        g.record(102, 2048 * MIB);
        assert_eq!(g.rise(102), Some((2048 - 130) * MIB));
        assert_eq!(g.hours(102), 3);
        assert_eq!(g.learned(102), None, "three hours are not enough yet");
        // Out of the window, the old low no longer counts.
        assert_eq!(g.largest(102), Some(((2048 - 130) * MIB, 102)));
        assert_eq!(g.at_rest(102), Some(130 * MIB));
        let later = 100 + window_hours();
        g.record(later, 2048 * MIB);
        assert_eq!(g.rise(later), Some((2048 - 150) * MIB));
        assert_eq!(g.at_rest(later), Some(150 * MIB));
        g.forget();
        assert_eq!(
            (g.rise(later), g.at_rest(later), g.hours(later)),
            (None, None, 0)
        );
    }

    #[test]
    fn learned_after_the_hours_needed() {
        let mut g = Growth::default();
        let n = HOURS_NEEDED;
        for h in 0..n {
            g.record(1000 + h as i64, 130 * MIB + h * MIB);
        }
        assert_eq!(g.learned(1000 + n as i64 - 1), Some((n - 1) * MIB));
    }
}
