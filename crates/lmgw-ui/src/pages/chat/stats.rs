//! The stats row of a reply, from the gateway's `Timings`: the `stats` frame
//! while it streams, and the `timings` of its `done`.

use lmgw_api_types::chat_frames::Timings;

use super::Stats;

/// A count the page holds as `i64`.
fn count(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl Stats {
    /// Map a llama.cpp `timings` block (cache_n prompt tokens were KV-reused;
    /// full prompt = processed + cached). First-token decode rates are noise.
    pub(in crate::pages) fn from_timings(t: &Timings, ctx_max: Option<i64>) -> Stats {
        let cached = t.cache_n.map_or(0, count);
        let predicted_n = count(t.predicted_n);
        Stats {
            server: true,
            ctx_max,
            prefill: t.prompt_per_second,
            tps: if predicted_n >= 2 {
                t.predicted_per_second
            } else {
                None
            },
            prompt_tokens: Some(count(t.prompt_n) + cached),
            completion_tokens: Some(predicted_n),
            cached: (cached > 0).then_some(cached),
            draft_n: t.draft_n.map(count).filter(|n| *n > 0),
            draft_accepted: t.draft_n_accepted.map(count),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timings() -> Timings {
        Timings {
            prompt_n: 20,
            prompt_ms: Some(40.0),
            prompt_per_second: Some(500.0),
            predicted_n: 30,
            predicted_ms: Some(600.0),
            predicted_per_second: Some(50.0),
            cache_n: Some(100),
            draft_n: Some(10),
            draft_n_accepted: Some(7),
        }
    }

    #[test]
    fn the_row_counts_the_cache_in_the_prompt() {
        let s = Stats::from_timings(&timings(), Some(8192));
        assert_eq!(
            (s.prompt_tokens, s.completion_tokens, s.cached, s.ctx_max),
            (Some(120), Some(30), Some(100), Some(8192))
        );
        assert_eq!((s.prefill, s.tps), (Some(500.0), Some(50.0)));
        assert_eq!((s.draft_n, s.draft_accepted), (Some(10), Some(7)));
        assert!(s.server && !s.live);
    }

    #[test]
    fn the_first_token_has_no_decode_rate_and_nothing_cached_or_drafted_is_left_out() {
        let t = Timings {
            predicted_n: 1,
            cache_n: None,
            draft_n: Some(0),
            draft_n_accepted: None,
            ..timings()
        };
        let s = Stats::from_timings(&t, None);
        assert_eq!(
            (s.tps, s.cached, s.draft_n, s.draft_accepted),
            (None, None, None, None)
        );
        assert_eq!(s.prompt_tokens, Some(20));
    }

    #[test]
    fn an_absent_rate_shows_no_rate() {
        // A rate llama.cpp gave no number for is left out of the frame; an
        // older stored one reads `null` the same.
        let sent = serde_json::json!({
            "prompt_n": 20, "prompt_ms": 40.0, "prompt_per_second": null,
            "predicted_n": 30, "predicted_ms": 600.0, "predicted_per_second": null
        });
        let t: Timings = serde_json::from_value(sent).expect("a null rate reads");
        let s = Stats::from_timings(&t, None);
        assert_eq!((s.prefill, s.tps), (None, None));
        assert_eq!((s.prompt_tokens, s.completion_tokens), (Some(20), Some(30)));
    }
}
