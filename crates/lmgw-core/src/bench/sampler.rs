//! The run's GPU sampler (benchmark design §4.3): device VRAM used, power,
//! temperature and NVML's clock-event reasons every 100 ms, for the whole
//! run — the integration starts it before the load phase and the engine
//! marks the phases on it.
//!
//! **Three uses of one series.**
//!
//! * *Peaks*: the largest VRAM and power seen at any tick.
//! * *The stored timeline*: 500 ms buckets (VRAM and temperature at their
//!   maximum, power averaged, clock events OR-ed), with phase bands.
//! * *Energy* for any window `[a, b]` after the fact: the driver's total
//!   energy counter interpolated at both ends when the card has one, else
//!   the power samples integrated as trapezoids ([`EnergySource`] records
//!   which). This is what lets a decode point measure first token → last
//!   token without reading the driver at those exact instants.
//!
//! **Windows too short to resolve get no figure** (§13 decision 56). Both
//! ends of a window are interpolated between ticks, so a window inside one
//! tick is the neighbouring ticks' average power times its length — not a
//! measurement. A window must span [`MIN_WINDOW_TICKS`] ticks, which puts at
//! least one whole tick of counter (or power) readings inside it; a point
//! with a shorter window gets no energy, and says why
//! ([`EnergyReading::TooShort`]). Prefill's TTFT windows at 512–2048 tokens
//! (15–60 ms on a 4090) are the ones this catches.
//!
//! **Sums over devices.** Every device the probe lists counts: VRAM, power and
//! energy are summed, temperature is the hottest, clock events are OR-ed. A
//! reading one device cannot give makes the sum unknown rather than
//! understated. (Per-device figures for multi-GPU runs are §11.)
//!
//! **No telemetry, no numbers.** A probe that answers nothing (no NVML, CI's
//! `NoTelemetry`) leaves the series empty, and every energy figure is `None`
//! — never zero joules.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lmgw_api_types::bench::{
    Energy, EnergySource, EnergySummary, GpuIdentity, Phase, PhaseSpan, Timeline, TimelineSample,
    SAMPLE_INTERVAL_MS, THROTTLE_MASK, TIMELINE_INTERVAL_MS,
};

use crate::vram::{GpuMemory, GpuPower, GpuProbe};

/// The tick (§4.3).
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(SAMPLE_INTERVAL_MS);

/// The stored timeline's bucket (§4.3).
pub const TIMELINE_INTERVAL: Duration = Duration::from_millis(TIMELINE_INTERVAL_MS);

/// The shortest window the sampler measures, in its own ticks (decision
/// 56): two ticks put at least one whole tick of readings inside any
/// window, so no more than half of it rests on interpolation. At the 100 ms
/// tick that is 200 ms.
pub const MIN_WINDOW_TICKS: u32 = 2;

/// A point's energy as the sampler can give it.
#[derive(Debug, Clone, PartialEq)]
pub enum EnergyReading {
    Measured(Energy),
    /// A window was shorter than [`Sampler::min_window`]: the reason, for
    /// the point's `energy_unmeasured`.
    TooShort(String),
    /// No telemetry, or the series does not cover a window: no figure and
    /// nothing to add (the run's energy summary says why there is none).
    Unavailable,
}

impl EnergyReading {
    /// `(energy, energy_unmeasured)` as the points store them.
    pub fn into_parts(self) -> (Option<Energy>, Option<String>) {
        match self {
            EnergyReading::Measured(e) => (Some(e), None),
            EnergyReading::TooShort(why) => (None, Some(why)),
            EnergyReading::Unavailable => (None, None),
        }
    }
}

/// Why `windows` cannot be measured at a resolution of `min`, when the
/// shortest of them is shorter; `tick` is named in the reason.
pub fn too_short(windows: &[EnergyWindow], min: Duration, tick: Duration) -> Option<String> {
    let shortest = windows
        .iter()
        .map(|w| w.end.saturating_duration_since(w.start))
        .min()?;
    (shortest < min).then(|| {
        format!(
            "too short to measure: the shortest window is {:.0} ms, and the GPU sampler resolves \
             windows of {} ms or more ({MIN_WINDOW_TICKS} of its {} ms ticks)",
            shortest.as_secs_f64() * 1000.0,
            min.as_millis(),
            tick.as_millis()
        )
    })
}

/// One tick's reading, summed over the devices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub at: Instant,
    pub vram_used: Option<u64>,
    pub power_mw: Option<u64>,
    /// The driver's total-energy counter, millijoules.
    pub energy_mj: Option<u64>,
    pub temp_c: Option<u32>,
    pub clock_events: Option<u64>,
}

impl Sample {
    /// Fold one reading of every device. `None` when neither half answered.
    pub fn from_readings(
        at: Instant,
        mem: Option<&[GpuMemory]>,
        power: Option<&[GpuPower]>,
    ) -> Option<Sample> {
        let mem = mem.filter(|m| !m.is_empty());
        let power = power.filter(|p| !p.is_empty());
        if mem.is_none() && power.is_none() {
            return None;
        }
        fn all<T, U: Copy>(xs: &[T], f: impl Fn(&T) -> Option<U>) -> Option<Vec<U>> {
            xs.iter().map(f).collect()
        }
        let p = |f: fn(&GpuPower) -> Option<u64>| power.and_then(|ps| all(ps, f));
        Some(Sample {
            at,
            vram_used: mem.map(|m| m.iter().map(|d| d.used_bytes).sum()),
            power_mw: p(|d| d.power_mw.map(u64::from)).map(|v| v.iter().sum()),
            energy_mj: p(|d| d.energy_mj).map(|v| v.iter().sum()),
            temp_c: power
                .and_then(|ps| all(ps, |d| d.temperature_c))
                .and_then(|v| v.into_iter().max()),
            clock_events: p(|d| d.clock_events).map(|v| v.iter().fold(0, |a, b| a | b)),
        })
    }
}

/// A measured window and the tokens it produced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnergyWindow {
    pub start: Instant,
    pub end: Instant,
    pub tokens: u64,
}

/// The card before the bench container starts (§3.2 step 4).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Baseline {
    /// Median device VRAM used over the window.
    pub vram_used_bytes: Option<u64>,
    /// Mean power over the window.
    pub idle_power_w: Option<f64>,
}

#[derive(Debug, Clone, Default)]
struct DeviceInfo {
    name: Option<String>,
    devices: u32,
    vram_total: Option<u64>,
    power_limit_mw: Option<u64>,
    driver: Option<String>,
}

#[derive(Default)]
struct State {
    samples: Vec<Sample>,
    phases: Vec<PhaseSpan>,
    info: Option<DeviceInfo>,
    power_error: Option<String>,
    ticks: u64,
}

struct Shared {
    epoch: Instant,
    telemetry: String,
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The running sampler. Dropping it stops the task.
pub struct Sampler {
    shared: Arc<Shared>,
    interval: Duration,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Sampler {
    /// Start sampling `probe` every [`SAMPLE_INTERVAL`]; the first reading is
    /// taken at once.
    pub fn start(probe: Arc<dyn GpuProbe>) -> Self {
        Self::with_interval(probe, SAMPLE_INTERVAL)
    }

    /// The same at another tick — the tests' seam.
    pub fn with_interval(probe: Arc<dyn GpuProbe>, interval: Duration) -> Self {
        let shared = Arc::new(Shared {
            epoch: Instant::now(),
            telemetry: probe.source(),
            state: Mutex::new(State::default()),
        });
        let task = tokio::spawn(tick_loop(probe, shared.clone(), interval));
        Self {
            shared,
            interval,
            task,
        }
    }

    /// When sampling started: the timeline's zero.
    pub fn epoch(&self) -> Instant {
        self.shared.epoch
    }

    /// Close the running phase's band and open `phase`'s.
    pub fn mark(&self, phase: Phase) {
        let now = self.ms_since_epoch(Instant::now());
        let mut st = self.shared.lock();
        close_open(&mut st.phases, now);
        st.phases.push(PhaseSpan {
            phase,
            start_ms: now,
            end_ms: None,
        });
    }

    /// Close the running phase's band.
    pub fn end_phase(&self) {
        let now = self.ms_since_epoch(Instant::now());
        close_open(&mut self.shared.lock().phases, now);
    }

    pub fn samples(&self) -> Vec<Sample> {
        self.shared.lock().samples.clone()
    }

    pub fn latest(&self) -> Option<Sample> {
        self.shared.lock().samples.last().copied()
    }

    /// Wait until the series covers `t` — a sample at or after it — so a
    /// window that just ended can be measured. Returns at once when the probe
    /// reports nothing at all, and gives up a few ticks past `t` if the probe
    /// has stopped answering (the energy is then `None`, not a guess).
    pub async fn settle(&self, t: Instant) {
        let grace = (self.interval * 5).max(Duration::from_secs(1));
        loop {
            let (covered, dead) = {
                let st = self.shared.lock();
                (
                    st.samples.last().is_some_and(|s| s.at >= t),
                    st.ticks > 0 && st.samples.is_empty(),
                )
            };
            if covered || dead || Instant::now() > t + grace {
                return;
            }
            tokio::time::sleep(self.interval / 2).await;
        }
    }

    /// The shortest window [`Self::energy`] measures: [`MIN_WINDOW_TICKS`]
    /// of this sampler's ticks.
    pub fn min_window(&self) -> Duration {
        self.interval * MIN_WINDOW_TICKS
    }

    /// The energy of `windows`, summed ([`energy_over`]) — or why there is
    /// none: a window shorter than [`Self::min_window`] is not measured
    /// (decision 56), and neither is a series that does not cover them.
    pub async fn energy(&self, windows: &[EnergyWindow]) -> EnergyReading {
        let Some(end) = windows.iter().map(|w| w.end).max() else {
            return EnergyReading::Unavailable;
        };
        self.settle(end).await;
        let st = self.shared.lock();
        // "Too short" only where there is something to measure at all.
        if !st
            .samples
            .iter()
            .any(|s| s.power_mw.is_some() || s.energy_mj.is_some())
        {
            return EnergyReading::Unavailable;
        }
        if let Some(why) = too_short(windows, self.min_window(), self.interval) {
            return EnergyReading::TooShort(why);
        }
        energy_over(&st.samples, windows)
            .map_or(EnergyReading::Unavailable, EnergyReading::Measured)
    }

    /// VRAM and idle power over `[from, to]` (§3.2 step 4).
    pub async fn baseline(&self, from: Instant, to: Instant) -> Baseline {
        self.settle(to).await;
        baseline(&self.shared.lock().samples, from, to)
    }

    /// The stored timeline so far.
    pub fn timeline(&self) -> Timeline {
        let st = self.shared.lock();
        Timeline {
            interval_ms: TIMELINE_INTERVAL_MS,
            samples: downsample(&st.samples, self.shared.epoch, TIMELINE_INTERVAL),
            phases: st.phases.clone(),
        }
    }

    /// §6 `gpu` so far.
    pub fn gpu_identity(&self) -> GpuIdentity {
        let st = self.shared.lock();
        let info = st.info.clone().unwrap_or_default();
        let temps: Vec<u32> = st.samples.iter().filter_map(|s| s.temp_c).collect();
        let clock_events = st
            .samples
            .iter()
            .filter_map(|s| s.clock_events)
            .fold(0, |a, b| a | b);
        GpuIdentity {
            name: info.name,
            driver: info.driver,
            devices: info.devices,
            vram_total_bytes: info.vram_total,
            power_limit_w: info.power_limit_mw.map(|mw| mw as f64 / 1000.0),
            temp_start_c: temps.first().copied(),
            temp_end_c: temps.last().copied(),
            temp_max_c: temps.iter().copied().max(),
            clock_events,
            throttled: clock_events & THROTTLE_MASK != 0,
            telemetry: self.shared.telemetry.clone(),
        }
    }

    /// Fill `e`'s measured half — source, peaks, total — leaving the
    /// baseline fields to whoever measured the baseline.
    pub fn fill_energy_summary(&self, e: &mut EnergySummary) {
        let st = self.shared.lock();
        let s = &st.samples;
        e.source = if s.iter().any(|x| x.energy_mj.is_some()) {
            Some(EnergySource::Counter)
        } else if s.iter().any(|x| x.power_mw.is_some()) {
            Some(EnergySource::Integrated)
        } else {
            None
        };
        e.unavailable = match e.source {
            Some(_) => None,
            None => Some(
                st.power_error
                    .clone()
                    .unwrap_or_else(|| format!("{}: no power readings", self.shared.telemetry)),
            ),
        };
        e.peak_vram_bytes = s.iter().filter_map(|x| x.vram_used).max();
        e.peak_power_w = s
            .iter()
            .filter_map(|x| x.power_mw)
            .max()
            .map(|mw| mw as f64 / 1000.0);
        e.total_joules = match (s.first(), s.last()) {
            (Some(a), Some(b)) if b.at > a.at => energy_between(s, a.at, b.at).map(|(j, _)| j),
            _ => None,
        };
    }

    fn ms_since_epoch(&self, t: Instant) -> u64 {
        t.saturating_duration_since(self.shared.epoch).as_millis() as u64
    }
}

fn close_open(phases: &mut [PhaseSpan], now: u64) {
    if let Some(open) = phases.last_mut().filter(|p| p.end_ms.is_none()) {
        open.end_ms = Some(now);
    }
}

async fn tick_loop(probe: Arc<dyn GpuProbe>, shared: Arc<Shared>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let first = shared.lock().info.is_none();
        let p = probe.clone();
        let read = tokio::task::spawn_blocking(move || {
            let at = Instant::now();
            let mem = p.devices();
            let power = p.power();
            let driver = if first { p.driver_version() } else { None };
            (at, mem, power, driver)
        })
        .await;
        let Ok((at, mem, power, driver)) = read else {
            continue;
        };
        let mut st = shared.lock();
        st.ticks += 1;
        if st.info.is_none() {
            let m = mem.as_deref().ok();
            let pw = power.as_deref().ok();
            st.info = Some(DeviceInfo {
                name: m.and_then(|m| m.first()).map(|d| d.name.clone()),
                devices: m.map(|m| m.len()).or(pw.map(|p| p.len())).unwrap_or(0) as u32,
                vram_total: m.map(|m| m.iter().map(|d| d.total_bytes).sum()),
                power_limit_mw: pw.and_then(|p| {
                    p.iter()
                        .map(|d| d.power_limit_mw.map(u64::from))
                        .sum::<Option<u64>>()
                }),
                driver,
            });
        }
        if let Err(e) = &power {
            st.power_error = Some(e.clone());
        }
        if let Some(s) = Sample::from_readings(at, mem.as_deref().ok(), power.as_deref().ok()) {
            st.samples.push(s);
        }
    }
}

/// The energy counter at `t`, interpolated between the samples around it
/// (millijoules). `None` outside the covered range or without a counter.
fn counter_at(samples: &[Sample], t: Instant) -> Option<f64> {
    let e: Vec<(Instant, u64)> = samples
        .iter()
        .filter_map(|s| s.energy_mj.map(|mj| (s.at, mj)))
        .collect();
    let idx = e.partition_point(|(at, _)| *at <= t);
    let (lo_at, lo) = *e.get(idx.checked_sub(1)?)?;
    if lo_at == t {
        return Some(lo as f64);
    }
    let (hi_at, hi) = *e.get(idx)?;
    let frac = (t - lo_at).as_secs_f64() / (hi_at - lo_at).as_secs_f64();
    Some(lo as f64 + (hi as f64 - lo as f64) * frac)
}

/// Power at `t`, interpolated (milliwatts).
fn power_at(p: &[(Instant, f64)], t: Instant) -> Option<f64> {
    let idx = p.partition_point(|(at, _)| *at <= t);
    let (lo_at, lo) = *p.get(idx.checked_sub(1)?)?;
    if lo_at == t {
        return Some(lo);
    }
    let (hi_at, hi) = *p.get(idx)?;
    let frac = (t - lo_at).as_secs_f64() / (hi_at - lo_at).as_secs_f64();
    Some(lo + (hi - lo) * frac)
}

/// Power samples over `[a, b]` as trapezoids (joules). `None` unless samples
/// on both sides cover the window.
fn integrate(samples: &[Sample], a: Instant, b: Instant) -> Option<f64> {
    let p: Vec<(Instant, f64)> = samples
        .iter()
        .filter_map(|s| s.power_mw.map(|mw| (s.at, mw as f64)))
        .collect();
    let mut pts = vec![(a, power_at(&p, a)?)];
    pts.extend(p.iter().copied().filter(|(at, _)| *at > a && *at < b));
    pts.push((b, power_at(&p, b)?));
    let mj: f64 = pts
        .windows(2)
        .map(|w| (w[0].1 + w[1].1) / 2.0 * (w[1].0 - w[0].0).as_secs_f64())
        .sum();
    Some(mj / 1000.0)
}

/// The joules spent in `[a, b]` and how they were measured: the counter when
/// it covers both ends (and did not go backwards — a driver reload resets
/// it), else the integrated power.
pub fn energy_between(samples: &[Sample], a: Instant, b: Instant) -> Option<(f64, EnergySource)> {
    if b < a {
        return None;
    }
    if let (Some(ea), Some(eb)) = (counter_at(samples, a), counter_at(samples, b)) {
        if eb >= ea {
            return Some(((eb - ea) / 1000.0, EnergySource::Counter));
        }
    }
    integrate(samples, a, b).map(|j| (j, EnergySource::Integrated))
}

/// The energy of several windows, summed: joules, seconds, tokens, average
/// power and tokens per joule. `None` when any window cannot be measured —
/// a sum over the measurable ones would be a smaller number presented as
/// the whole. The source is [`EnergySource::Integrated`] if any window was.
pub fn energy_over(samples: &[Sample], windows: &[EnergyWindow]) -> Option<Energy> {
    if windows.is_empty() {
        return None;
    }
    let mut out = Energy::default();
    let mut each = Vec::with_capacity(windows.len());
    for w in windows {
        let (j, source) = energy_between(samples, w.start, w.end)?;
        if j > 0.0 {
            each.push(w.tokens as f64 / j);
        }
        out.joules += j;
        out.seconds += (w.end - w.start).as_secs_f64();
        out.tokens += w.tokens;
        if source == EnergySource::Integrated {
            out.source = EnergySource::Integrated;
        }
    }
    out.avg_w = if out.seconds > 0.0 {
        out.joules / out.seconds
    } else {
        0.0
    };
    out.tokens_per_joule = (out.joules > 0.0).then(|| out.tokens as f64 / out.joules);
    out.tokens_per_joule_each = super::stats::stat(&each);
    Some(out)
}

/// §3.2 step 4 over a sample series.
pub fn baseline(samples: &[Sample], from: Instant, to: Instant) -> Baseline {
    let inside: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.at >= from && s.at <= to)
        .collect();
    let vram: Vec<f64> = inside
        .iter()
        .filter_map(|s| s.vram_used.map(|v| v as f64))
        .collect();
    let secs = to.saturating_duration_since(from).as_secs_f64();
    let idle_power_w = energy_between(samples, from, to)
        .filter(|_| secs > 0.0)
        .map(|(j, _)| j / secs)
        .or_else(|| {
            let p: Vec<f64> = inside
                .iter()
                .filter_map(|s| s.power_mw.map(|mw| mw as f64 / 1000.0))
                .collect();
            (!p.is_empty()).then(|| p.iter().sum::<f64>() / p.len() as f64)
        });
    Baseline {
        vram_used_bytes: super::stats::median(&vram).map(|v| v as u64),
        idle_power_w,
    }
}

/// The stored timeline: one entry per `every` bucket since `epoch` that has
/// samples — VRAM and temperature at their bucket maximum, power averaged,
/// clock events OR-ed.
pub fn downsample(samples: &[Sample], epoch: Instant, every: Duration) -> Vec<TimelineSample> {
    let every_ms = every.as_millis().max(1) as u64;
    let mut out: Vec<TimelineSample> = Vec::new();
    let mut power: Vec<f64> = Vec::new();
    let mut current: Option<u64> = None;
    let flush = |out: &mut Vec<TimelineSample>, power: &mut Vec<f64>| {
        if let Some(last) = out.last_mut() {
            last.power_w =
                (!power.is_empty()).then(|| power.iter().sum::<f64>() / power.len() as f64);
        }
        power.clear();
    };
    for s in samples {
        let bucket = s.at.saturating_duration_since(epoch).as_millis() as u64 / every_ms;
        if current != Some(bucket) {
            flush(&mut out, &mut power);
            current = Some(bucket);
            out.push(TimelineSample {
                t_ms: bucket * every_ms,
                ..Default::default()
            });
        }
        let cur = out.last_mut().expect("pushed above");
        cur.vram_used_bytes = cur.vram_used_bytes.max(s.vram_used);
        cur.temp_c = cur.temp_c.max(s.temp_c);
        cur.clock_events = match (cur.clock_events, s.clock_events) {
            (Some(a), Some(b)) => Some(a | b),
            (a, b) => a.or(b),
        };
        if let Some(mw) = s.power_mw {
            power.push(mw as f64 / 1000.0);
        }
    }
    flush(&mut out, &mut power);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t0: Instant, ms: u64, power_w: u64, energy_j: Option<u64>) -> Sample {
        Sample {
            at: t0 + Duration::from_millis(ms),
            vram_used: Some(1000 + ms),
            power_mw: Some(power_w * 1000),
            energy_mj: energy_j.map(|j| j * 1000),
            temp_c: Some(40 + (ms / 1000) as u32),
            clock_events: Some(if ms == 300 { 0x4 } else { 0x1 }),
        }
    }

    /// 100 W for a second: the counter says 100 J, and so does integrating.
    fn steady(t0: Instant, counter: bool) -> Vec<Sample> {
        (0..=10)
            .map(|i| sample(t0, i * 100, 100, counter.then_some(5000 + i * 10)))
            .collect()
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn the_counter_is_interpolated_at_both_ends() {
        let t0 = Instant::now();
        let s = steady(t0, true);
        let (j, src) = energy_between(&s, at(t0, 150), at(t0, 650)).unwrap();
        assert_eq!(src, EnergySource::Counter);
        assert!((j - 50.0).abs() < 1e-9, "{j}");
        // Outside the covered range: no counter, no power either.
        assert_eq!(energy_between(&s, at(t0, 900), at(t0, 1100)), None);
        assert_eq!(energy_between(&s, at(t0, 500), at(t0, 400)), None);
    }

    #[test]
    fn without_a_counter_power_is_integrated() {
        let t0 = Instant::now();
        let mut s = steady(t0, false);
        let (j, src) = energy_between(&s, at(t0, 150), at(t0, 650)).unwrap();
        assert_eq!(src, EnergySource::Integrated);
        assert!((j - 50.0).abs() < 1e-9, "{j}");
        // A ramp: 0 W → 200 W over a second is 100 J.
        for (i, x) in s.iter_mut().enumerate() {
            x.power_mw = Some(i as u64 * 20_000);
        }
        let (j, _) = energy_between(&s, at(t0, 0), at(t0, 1000)).unwrap();
        assert!((j - 100.0).abs() < 1e-9, "{j}");
    }

    #[test]
    fn a_counter_that_went_backwards_falls_back_to_power() {
        let t0 = Instant::now();
        let mut s = steady(t0, true);
        // A driver reload mid-window: the counter restarts from zero.
        for (i, x) in s.iter_mut().enumerate().skip(5) {
            x.energy_mj = Some((i as u64 - 5) * 10_000);
        }
        let (_, src) = energy_between(&s, at(t0, 450), at(t0, 650)).unwrap();
        assert_eq!(src, EnergySource::Integrated);
    }

    #[test]
    fn windows_sum_and_any_integrated_window_marks_the_whole() {
        let t0 = Instant::now();
        let s = steady(t0, true);
        let w = |a, b, tokens| EnergyWindow {
            start: at(t0, a),
            end: at(t0, b),
            tokens,
        };
        let e = energy_over(&s, &[w(0, 200, 30), w(500, 800, 70)]).unwrap();
        assert!((e.joules - 50.0).abs() < 1e-9, "{e:?}");
        assert!((e.seconds - 0.5).abs() < 1e-9);
        assert!((e.avg_w - 100.0).abs() < 1e-9);
        assert_eq!(e.tokens, 100);
        assert_eq!(e.tokens_per_joule, Some(2.0));
        assert_eq!(e.source, EnergySource::Counter);
        // One window outside the series: the whole figure is unknown.
        assert_eq!(energy_over(&s, &[w(0, 200, 1), w(900, 1500, 1)]), None);
        assert_eq!(energy_over(&s, &[]), None);
        // A zero-length window measures 0 J: no tokens per joule.
        let e = energy_over(&s, &[w(300, 300, 5)]).unwrap();
        assert_eq!(e.tokens_per_joule, None);
    }

    /// Review finding 3: a prefill TTFT window of 15–60 ms lies inside one
    /// 100 ms tick, and its "energy" was the ticks' average power times its
    /// length. A window must span two ticks.
    #[test]
    fn a_window_shorter_than_two_ticks_is_too_short() {
        let t0 = Instant::now();
        let tick = Duration::from_millis(100);
        let min = tick * MIN_WINDOW_TICKS;
        let w = |a, b| EnergyWindow {
            start: at(t0, a),
            end: at(t0, b),
            tokens: 512,
        };
        let why = too_short(&[w(0, 250), w(300, 342)], min, tick).unwrap();
        assert!(why.starts_with("too short to measure"), "{why}");
        assert!(
            why.contains("42 ms") && why.contains("200 ms") && why.contains("100 ms ticks"),
            "{why}"
        );
        assert_eq!(too_short(&[w(0, 200), w(300, 700)], min, tick), None);
        assert_eq!(too_short(&[], min, tick), None);
        let parts = EnergyReading::TooShort(why.clone()).into_parts();
        assert_eq!(parts, (None, Some(why)));
        assert_eq!(EnergyReading::Unavailable.into_parts(), (None, None));
    }

    #[test]
    fn the_timeline_buckets_at_500_ms() {
        let t0 = Instant::now();
        let s = steady(t0, true);
        let tl = downsample(&s, t0, Duration::from_millis(500));
        assert_eq!(
            tl.iter().map(|x| x.t_ms).collect::<Vec<_>>(),
            vec![0, 500, 1000]
        );
        assert_eq!(tl[0].vram_used_bytes, Some(1400), "bucket maximum");
        assert_eq!(tl[0].power_w, Some(100.0));
        assert_eq!(tl[0].clock_events, Some(0x5), "OR over the bucket");
        assert_eq!(tl[1].clock_events, Some(0x1));
        assert_eq!(tl[2].temp_c, Some(41));
    }

    #[test]
    fn the_baseline_is_median_vram_and_mean_power() {
        let t0 = Instant::now();
        let s = steady(t0, true);
        let b = baseline(&s, at(t0, 0), at(t0, 1000));
        assert_eq!(b.vram_used_bytes, Some(1500));
        assert!((b.idle_power_w.unwrap() - 100.0).abs() < 1e-9);
        assert_eq!(baseline(&[], at(t0, 0), at(t0, 1000)), Baseline::default());
    }

    #[test]
    fn readings_fold_over_devices_and_a_missing_one_is_unknown() {
        let t0 = Instant::now();
        let mem = |used| GpuMemory {
            index: 0,
            name: "GPU".into(),
            total_bytes: 100,
            used_bytes: used,
            free_bytes: 100 - used,
        };
        let pw = |mw: u32, mj: Option<u64>, t: u32, ev: u64| GpuPower {
            index: 0,
            power_mw: Some(mw),
            energy_mj: mj,
            temperature_c: Some(t),
            clock_events: Some(ev),
            power_limit_mw: Some(450_000),
        };
        let s = Sample::from_readings(
            t0,
            Some(&[mem(10), mem(20)]),
            Some(&[pw(1000, Some(5), 40, 0x1), pw(2000, None, 60, 0x4)]),
        )
        .unwrap();
        assert_eq!(s.vram_used, Some(30));
        assert_eq!(s.power_mw, Some(3000));
        assert_eq!(s.energy_mj, None, "one device without a counter");
        assert_eq!(s.temp_c, Some(60));
        assert_eq!(s.clock_events, Some(0x5));
        assert_eq!(Sample::from_readings(t0, None, None), None);
        let only_mem = Sample::from_readings(t0, Some(&[mem(10)]), None).unwrap();
        assert_eq!((only_mem.vram_used, only_mem.power_mw), (Some(10), None));
    }
}
