//! The arithmetic of the phases (benchmark design §4.3), as pure functions
//! over numbers and client timestamps, so every formula is unit-tested on
//! synthetic data rather than trusted to a live server.

use std::time::{Duration, Instant};

use lmgw_api_types::bench::{DraftStats, Stat};

use crate::ir::Timings;

/// Median, min and max of `values`, with the raw values kept in their order.
/// An even count's median is the mean of the middle two. Empty in, zeros
/// out — a point with no repetition is never stored, so this is only a
/// guard.
pub fn stat(values: &[f64]) -> Stat {
    let Some(median) = median(values) else {
        return Stat::default();
    };
    Stat {
        median,
        min: values.iter().copied().fold(f64::INFINITY, f64::min),
        max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        values: values.to_vec(),
    }
}

pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let mid = v.len() / 2;
    Some(if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    })
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// One stream of a concurrent repetition, as the aggregate needs it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamSpan {
    pub first_token: Instant,
    pub end: Instant,
    pub predicted_n: u64,
}

/// **Aggregate** = Σ `predicted_n` / (last stream's end − first stream's
/// first token): decode throughput with the prefills excluded (§4.3). `None`
/// for no streams or a zero-length window.
pub fn aggregate_tok_s(streams: &[StreamSpan]) -> Option<f64> {
    let first = streams.iter().map(|s| s.first_token).min()?;
    let last = streams.iter().map(|s| s.end).max()?;
    let secs = last.checked_duration_since(first)?.as_secs_f64();
    if secs <= 0.0 {
        return None;
    }
    let tokens: u64 = streams.iter().map(|s| s.predicted_n).sum();
    Some(tokens as f64 / secs)
}

/// Draft statistics summed over requests; `None` when no request reported
/// any (the server was not drafting).
pub fn draft_stats<'a>(timings: impl IntoIterator<Item = &'a Timings>) -> Option<DraftStats> {
    let mut seen = false;
    let (mut drafted, mut accepted) = (0u64, 0u64);
    for t in timings {
        if let Some(d) = t.draft_n {
            seen = true;
            drafted += d;
            accepted += t.draft_n_accepted.unwrap_or(0);
        }
    }
    seen.then(|| DraftStats {
        drafted,
        accepted,
        acceptance: (drafted > 0).then(|| accepted as f64 / drafted as f64),
    })
}

/// One mixed repetition's numbers (§4.3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MixedWindow {
    /// Median over the streams of each one's rate in `[inject − steady, inject)`.
    pub before_tok_s: f64,
    /// Median over the streams of each one's rate in `[inject, inject_first)`.
    pub during_tok_s: f64,
    /// The longest inter-token gap of any stream that overlaps
    /// `[inject, inject_first]`.
    pub stall_ms: f64,
    /// Streams whose last token came before `inject_first` and that never
    /// produced another: their trailing gap is counted up to `inject_first`
    /// only, so the stall is a lower bound for them.
    pub open_streams: usize,
}

/// The mixed phase's windows over each decoding stream's token timestamps
/// (`(arrival, tokens in that chunk)`, in arrival order).
///
/// A gap counts toward the stall when it overlaps the window at all, and at
/// its full length — a stream that last produced a token just before the
/// injection and next one well after it stalled for that whole gap. A stream
/// with no token after `inject_first` contributes its open gap up to
/// `inject_first`. `None` when there are no streams or the window is empty.
pub fn mixed_window(
    streams: &[Vec<(Instant, u32)>],
    inject: Instant,
    inject_first: Instant,
    steady: Duration,
) -> Option<MixedWindow> {
    if streams.is_empty() || inject_first <= inject {
        return None;
    }
    let before_start = inject.checked_sub(steady)?;
    let during_secs = (inject_first - inject).as_secs_f64();
    let mut before = Vec::with_capacity(streams.len());
    let mut during = Vec::with_capacity(streams.len());
    let mut stall = Duration::ZERO;
    let mut open_streams = 0;
    for tokens in streams {
        let count = |from: Instant, to: Instant| -> u64 {
            tokens
                .iter()
                .filter(|(t, _)| *t >= from && *t < to)
                .map(|(_, n)| *n as u64)
                .sum()
        };
        before.push(count(before_start, inject) as f64 / steady.as_secs_f64());
        during.push(count(inject, inject_first) as f64 / during_secs);

        for w in tokens.windows(2) {
            let (a, b) = (w[0].0, w[1].0);
            if b >= inject && a <= inject_first {
                stall = stall.max(b - a);
            }
        }
        match tokens.last() {
            Some((last, _)) if *last < inject_first => {
                open_streams += 1;
                stall = stall.max(inject_first - *last);
            }
            None => {
                open_streams += 1;
                stall = stall.max(inject_first - inject);
            }
            _ => {}
        }
    }
    Some(MixedWindow {
        before_tok_s: median(&before)?,
        during_tok_s: median(&during)?,
        stall_ms: ms(stall),
        open_streams,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn median_min_max() {
        let s = stat(&[3.0, 1.0, 2.0]);
        assert_eq!((s.median, s.min, s.max), (2.0, 1.0, 3.0));
        assert_eq!(
            s.values,
            vec![3.0, 1.0, 2.0],
            "raw values keep repetition order"
        );
        assert_eq!(stat(&[4.0, 1.0, 3.0, 2.0]).median, 2.5);
        assert_eq!(stat(&[7.0]).median, 7.0);
        assert_eq!(stat(&[]), Stat::default());
    }

    #[test]
    fn aggregate_excludes_the_prefills() {
        let t0 = Instant::now();
        // Two streams: first tokens at 100 and 150 ms (their prefills before
        // that are excluded), ends at 1100 and 1050 ms; 100 tokens each.
        let streams = [
            StreamSpan {
                first_token: at(t0, 100),
                end: at(t0, 1100),
                predicted_n: 100,
            },
            StreamSpan {
                first_token: at(t0, 150),
                end: at(t0, 1050),
                predicted_n: 100,
            },
        ];
        let agg = aggregate_tok_s(&streams).unwrap();
        assert!((agg - 200.0).abs() < 1e-9, "{agg}");
        assert_eq!(aggregate_tok_s(&[]), None);
        let zero = [StreamSpan {
            first_token: t0,
            end: t0,
            predicted_n: 5,
        }];
        assert_eq!(aggregate_tok_s(&zero), None);
    }

    #[test]
    fn draft_stats_sum_only_what_was_reported() {
        let t = |d: Option<u64>, a: Option<u64>| Timings {
            prompt_n: 0,
            prompt_ms: 0.0,
            prompt_per_second: 0.0,
            predicted_n: 0,
            predicted_ms: 0.0,
            predicted_per_second: 0.0,
            cache_n: None,
            draft_n: d,
            draft_n_accepted: a,
        };
        assert_eq!(draft_stats(&[t(None, None)]), None);
        let d = draft_stats(&[t(Some(10), Some(7)), t(Some(30), Some(13))]).unwrap();
        assert_eq!((d.drafted, d.accepted), (40, 20));
        assert_eq!(d.acceptance, Some(0.5));
        assert_eq!(draft_stats(&[t(Some(0), None)]).unwrap().acceptance, None);
    }

    /// Two streams at 10 ms per token; the injection at 2000 ms stalls both
    /// for 300 ms; the injected first token lands at 2400 ms.
    #[test]
    fn mixed_windows_and_the_stall() {
        let t0 = Instant::now() + Duration::from_secs(10);
        let stream = |offset: u64| -> Vec<(Instant, u32)> {
            let mut v: Vec<(Instant, u32)> =
                (0..=200).map(|i| (at(t0, offset + i * 10), 1)).collect(); // up to 2000(+offset) ms
                                                                           // stalled until 2300 ms, then back to 10 ms per token
            v.extend((0..=30).map(|i| (at(t0, 2300 + offset + i * 10), 1)));
            v
        };
        let streams = vec![stream(0), stream(5)];
        let w = mixed_window(
            &streams,
            at(t0, 2000),
            at(t0, 2400),
            Duration::from_millis(2000),
        )
        .unwrap();
        // 200 tokens in the 2 s before (stream 0: 0..1990 ms; stream 1: 5..1995 ms).
        assert!((w.before_tok_s - 100.0).abs() < 1e-9, "{w:?}");
        // In [2000, 2400): stream 0 has 2000 + 2300..2390 = 1 + 10 tokens,
        // stream 1 2005 + 2305..2395, the same 11: 27.5 tok/s each.
        assert!((w.during_tok_s - 27.5).abs() < 1e-9, "{w:?}");
        // The gap 2000 → 2300 (stream 0) and 2005 → 2305 (stream 1).
        assert!((w.stall_ms - 300.0).abs() < 1e-6, "{w:?}");
        assert_eq!(w.open_streams, 0);
    }

    #[test]
    fn a_gap_straddling_the_injection_counts_at_full_length() {
        let t0 = Instant::now() + Duration::from_secs(10);
        let streams = vec![vec![(at(t0, 1500), 1), (at(t0, 2600), 1)]];
        let w = mixed_window(&streams, at(t0, 2000), at(t0, 2100), Duration::from_secs(2)).unwrap();
        assert!((w.stall_ms - 1100.0).abs() < 1e-6, "{w:?}");
    }

    #[test]
    fn a_stream_that_went_quiet_is_an_open_gap() {
        let t0 = Instant::now() + Duration::from_secs(10);
        let streams = vec![vec![(at(t0, 1900), 1), (at(t0, 1990), 1)], vec![]];
        let w = mixed_window(&streams, at(t0, 2000), at(t0, 2500), Duration::from_secs(2)).unwrap();
        // Stream 0: open from 1990 to 2500; stream 1 never produced a token.
        assert!((w.stall_ms - 510.0).abs() < 1e-6, "{w:?}");
        assert_eq!(w.open_streams, 2);
        assert_eq!(w.during_tok_s, 0.0);
    }

    #[test]
    fn an_empty_window_has_no_numbers() {
        let t0 = Instant::now() + Duration::from_secs(10);
        assert!(mixed_window(&[vec![]], at(t0, 10), at(t0, 10), Duration::from_secs(2)).is_none());
        assert!(mixed_window(&[], at(t0, 10), at(t0, 20), Duration::from_secs(2)).is_none());
    }
}
