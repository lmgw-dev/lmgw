//! The points a run measures (benchmark design §4.2), as a pure function of
//! the per-slot context *S*, the slot count *N_slots*, whether the slots
//! share one KV pool, and the suite parameters (*G*, repetitions, the stream
//! and injection sizes).
//!
//! The top points come from the real context, so nothing is capped: a
//! 131k-context row gets a 131k prefill point (the no-hidden-limits
//! rule). Where a context is too small for a phase, the phase gets no points
//! and [`PointPlan::notes`] says why, instead of the plan quietly shrinking.
//!
//! **A shared pool** (unified KV, §13 decision 57): *S* is then the whole
//! pool, and requests that run together draw from it side by side — a full
//! pool aborts every running request. One request at a time (prefill,
//! decode, the probes) may use all of it, since llama-server clears idle
//! slots from a unified pool when a task starts. The concurrent streams and
//! the mixed phase's streams plus its injection must fit together: the
//! concurrent points stop where *N* streams no longer fit, and the mixed
//! injection is sized when the phase starts ([`size_mixed`]), from the decode
//! rate measured then.

use lmgw_api_types::bench::{MixedPlan, PointPlan, SuiteParams};

/// Derive every phase's points. `pool` is the shared KV pool's size when
/// the slots share one (unified KV), `None` when each slot has its own.
pub fn derive(
    per_slot_ctx: u64,
    n_slots: u32,
    pool: Option<u64>,
    params: &SuiteParams,
) -> PointPlan {
    let s = per_slot_ctx;
    let g = params.generate_tokens as u64;
    let mut notes = Vec::new();
    // Only several slots can share anything.
    let pool = pool.filter(|_| n_slots > 1);

    let prefill = prefill_points(s);
    if prefill.is_empty() {
        notes.push(format!(
            "prefill: a per-slot context of {s} tokens holds no prompt plus one generated token"
        ));
    }

    let decode = decode_points(s, g);
    if decode.is_empty() {
        notes.push(format!(
            "decode: a per-slot context of {s} tokens cannot hold {g} generated tokens after \
             any prompt"
        ));
    }

    let stream_prompt = params.stream_prompt_tokens;
    let stream_fits = stream_prompt + g < s;
    let concurrent = if stream_fits {
        let mut points = concurrent_points(n_slots);
        if let Some(pool) = pool {
            // N streams of prompt + G cells each, side by side.
            let fit = pool / (stream_prompt + g);
            if points.iter().any(|n| u64::from(*n) > fit) {
                points.retain(|n| u64::from(*n) <= fit);
                notes.push(format!(
                    "concurrent: the {n_slots} slots share one KV pool of {pool} tokens, which \
                     holds {fit} streams of a {stream_prompt}-token prompt plus {g} generated \
                     tokens side by side — more would overflow it, and a full pool aborts every \
                     running request"
                ));
            }
        }
        points
    } else {
        notes.push(format!(
            "concurrent: a per-slot context of {s} tokens cannot hold a {stream_prompt}-token \
             prompt plus {g} generated tokens"
        ));
        Vec::new()
    };

    let p_max = prefill.last().copied();
    let mixed = match (n_slots >= 2, stream_fits, p_max) {
        (true, true, Some(p_max)) => {
            let streams = n_slots - 1;
            let cap = params.mixed_inject_max_tokens.min(p_max);
            // On a shared pool the injection also has to fit beside the
            // decoding streams; how much they decode meanwhile is only known
            // once a rate is measured, so this is the bound before any
            // decoding (the phase sizes it for real, `size_mixed`).
            let room = match pool {
                None => Some(cap),
                Some(pool) => pool
                    .checked_sub(u64::from(streams) * (stream_prompt + MIXED_STREAM_SLACK) + 1)
                    .map(|r| r.min(cap))
                    .filter(|r| *r > 0),
            };
            match (room, pool) {
                (Some(inject), None) => Some(MixedPlan {
                    streams,
                    stream_prompt_tokens: stream_prompt,
                    stream_predict: s - stream_prompt - 1,
                    inject_tokens: inject,
                }),
                (Some(inject), Some(pool)) => {
                    notes.push(format!(
                        "mixed: the {n_slots} slots share one KV pool of {pool} tokens, so the \
                         injected prompt is sized when the phase starts: what the pool holds \
                         beside the {streams} decoding streams at the decode rate measured then \
                         (at most {inject} tokens)"
                    ));
                    Some(MixedPlan {
                        streams,
                        stream_prompt_tokens: stream_prompt,
                        stream_predict: s - stream_prompt - 1,
                        inject_tokens: inject,
                    })
                }
                (None, pool) => {
                    notes.push(format!(
                        "mixed: the {n_slots} slots share one KV pool of {} tokens, which cannot \
                         hold {streams} decoding streams of a {stream_prompt}-token prompt and an \
                         injected prompt beside them",
                        pool.unwrap_or(s)
                    ));
                    None
                }
            }
        }
        (false, ..) => {
            notes.push(format!(
                "mixed: needs at least two slots; this server has {n_slots}"
            ));
            None
        }
        _ => {
            notes.push(format!(
                "mixed: a per-slot context of {s} tokens cannot hold a decoding stream"
            ));
            None
        }
    };

    let needle_tokens = p_max
        .filter(|p| *p > params.needle_margin_tokens)
        .map(|p| p - params.needle_margin_tokens);
    if needle_tokens.is_none() {
        notes.push(format!(
            "needle: a per-slot context of {s} tokens leaves no room for a haystack beside the \
             {}-token margin",
            params.needle_margin_tokens
        ));
    }

    PointPlan {
        per_slot_ctx: s,
        n_slots,
        generate_tokens: params.generate_tokens,
        repetitions: params.repetitions,
        prefill,
        decode,
        concurrent,
        mixed,
        needle_tokens,
        provisional: false,
        notes,
    }
}

/// Tokens a mixed decoding stream holds beyond its prompt and what it
/// decodes at the measured rate: its first token, and the one the phase
/// waits for after the injected first token (the rate's own rounding is
/// rounded up separately).
pub const MIXED_STREAM_SLACK: u64 = 2;

/// What [`size_mixed`] decided.
#[derive(Debug, Clone, PartialEq)]
pub enum MixedSizing {
    /// Run with this plan; the note says how it was sized, when there is
    /// anything to say (a shared pool).
    Run(MixedPlan, Option<String>),
    /// A planned skip, and why: the phase is not an error.
    Skip(String),
}

/// The measured rates the mixed phase is sized from, tokens per second:
/// one stream's decode and its prompt's prefill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub decode: f64,
    pub prefill: f64,
}

/// Size the mixed phase from `planned` (§13 decision 57), once a decode and
/// a prefill rate are measured.
///
/// A decoding stream must keep decoding through the head start the others'
/// prompts give it, the steady window and the injected prefill, and one
/// token past it: `reserve(P)` = ⌈decode × (lead + steady + P / prefill)⌉ +
/// slack tokens. One stream's rate is an upper bound on each of several
/// streams' rates, so the reserve errs large.
///
/// * **Split KV:** the stream's slot must hold its prompt plus
///   `reserve(P_inj)`, or the stream ends inside the steady window and every
///   repetition fails; that is a planned skip instead.
/// * **Shared pool of `pool` cells:** the streams hold prompt +
///   `reserve(P)` each and the injected request P + 1, side by side, so
///   `P_inj` is the largest P (up to the plan's cap) that fits, and each
///   stream's `n_predict` is its reserve — a stream can then never hold more
///   than it was counted with. No P ≥ 1 fitting is a planned skip.
pub fn size_mixed(
    planned: &MixedPlan,
    per_slot_ctx: u64,
    pool: Option<u64>,
    steady_ms: u64,
    rates: Rates,
) -> MixedSizing {
    let n = f64::from(planned.streams);
    let prompt = planned.stream_prompt_tokens;
    let slot_room = per_slot_ctx.saturating_sub(prompt + 1);
    let steady = steady_ms as f64 / 1000.0;
    // The streams' prompts are prefilled one after another at worst: the
    // first stream decodes that long before the last one's first token.
    let lead = n * prompt as f64 / rates.prefill;
    let reserve = |p: u64| {
        (rates.decode * (lead + steady + p as f64 / rates.prefill)).ceil() as u64
            + MIXED_STREAM_SLACK
    };
    let at = format!(
        "at the measured {:.0} tok/s decode and {:.0} tok/s prefill",
        rates.decode, rates.prefill
    );
    let Some(pool) = pool else {
        let need = reserve(planned.inject_tokens);
        if need > slot_room {
            return MixedSizing::Skip(format!(
                "mixed: skipped — a per-slot context of {per_slot_ctx} tokens holds {slot_room} \
                 tokens beyond a stream's {prompt}-token prompt, and {at} a decoding stream needs \
                 about {need} to keep decoding through the {steady_ms} ms steady window and the \
                 {}-token injection",
                planned.inject_tokens
            ));
        }
        return MixedSizing::Run(planned.clone(), None);
    };
    // n·(prompt + reserve(P)) + P + 1 ≤ pool, with reserve(P) ≤
    // decode·(lead + steady) + decode/prefill·P + 1 + slack.
    let fixed = n
        * (prompt as f64 + rates.decode * (lead + steady) + 1.0 + MIXED_STREAM_SLACK as f64)
        + 1.0;
    let per_token = 1.0 + n * rates.decode / rates.prefill;
    let fit = ((pool as f64 - fixed) / per_token).floor();
    let inject = if fit >= 1.0 {
        (fit as u64).min(planned.inject_tokens)
    } else {
        0
    };
    let stream_predict = reserve(inject);
    if inject == 0 || stream_predict > slot_room {
        return MixedSizing::Skip(format!(
            "mixed: skipped — the slots share one KV pool of {pool} tokens, and {at} each of the \
             {} decoding streams needs about {} tokens beyond its {prompt}-token prompt through \
             the {steady_ms} ms steady window, which leaves no room for an injected prompt \
             beside them (a full pool aborts every running request)",
            planned.streams,
            reserve(1)
        ));
    }
    let note = format!(
        "mixed: the slots share one KV pool of {pool} tokens: {at}, each of the {} decoding \
         streams is given {stream_predict} tokens to generate (through the {steady_ms} ms steady \
         window and the injection), which leaves {inject} tokens to inject{}",
        planned.streams,
        if inject < planned.inject_tokens {
            format!(" (the cap is {})", planned.inject_tokens)
        } else {
            String::new()
        }
    );
    MixedSizing::Run(
        MixedPlan {
            stream_predict,
            inject_tokens: inject,
            ..planned.clone()
        },
        Some(note),
    )
}

/// *P* = 512·4ᵏ while *P* < *S* − 1, plus *P_max* = *S* − 2.
pub fn prefill_points(s: u64) -> Vec<u64> {
    if s < 3 {
        return Vec::new();
    }
    let p_max = s - 2;
    let mut out: Vec<u64> = std::iter::successors(Some(512u64), |p| p.checked_mul(4))
        .take_while(|p| *p < s - 1)
        .collect();
    if out.last() != Some(&p_max) {
        out.push(p_max);
    }
    out
}

/// *D* = 64, then 1024·4ᵏ, while *D* + *G* < *S*, plus *D_max* = *S* − *G* − 1.
pub fn decode_points(s: u64, g: u64) -> Vec<u64> {
    if s <= g + 1 {
        return Vec::new();
    }
    let d_max = s - g - 1;
    let mut out: Vec<u64> = std::iter::once(64u64)
        .chain(std::iter::successors(Some(1024u64), |d| d.checked_mul(4)))
        .take_while(|d| d + g < s)
        .collect();
    if out.last() != Some(&d_max) {
        out.push(d_max);
    }
    out
}

/// *N* = 1, 2, 4, … < *N_slots*, plus *N_slots*.
pub fn concurrent_points(n_slots: u32) -> Vec<u32> {
    if n_slots == 0 {
        return Vec::new();
    }
    let mut out: Vec<u32> = std::iter::successors(Some(1u32), |n| n.checked_mul(2))
        .take_while(|n| *n < n_slots)
        .collect();
    out.push(n_slots);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::bench::Phase;

    fn params() -> SuiteParams {
        SuiteParams::v1(3, Phase::ALL.to_vec())
    }

    #[test]
    fn a_long_context_gets_every_power_and_its_own_top() {
        let p = derive(131_072, 2, None, &params());
        assert_eq!(p.prefill, vec![512, 2048, 8192, 32768, 131_070]);
        assert_eq!(p.decode, vec![64, 1024, 4096, 16384, 65536, 130_815]);
        assert_eq!(p.concurrent, vec![1, 2]);
        let m = p.mixed.clone().unwrap();
        assert_eq!((m.streams, m.inject_tokens), (1, 8192));
        assert_eq!(m.stream_predict, 131_072 - 256 - 1);
        assert_eq!(p.needle_tokens, Some(131_070 - 512));
        assert_eq!(p.p_max(), Some(131_070));
        assert!(p.notes.is_empty(), "{:?}", p.notes);
        assert!(!p.provisional);
    }

    #[test]
    fn the_top_point_is_not_duplicated_when_it_lands_on_a_power() {
        // S − 2 = 2048 is itself 512·4: no second 2048.
        assert_eq!(prefill_points(2050), vec![512, 2048]);
        // S − G − 1 = 1024: no second 1024.
        assert_eq!(decode_points(1024 + 256 + 1, 256), vec![64, 1024]);
    }

    #[test]
    fn a_power_equal_to_s_minus_one_is_not_a_point() {
        // P < S − 1: with S = 2049, 2048 is excluded and P_max = 2047.
        assert_eq!(prefill_points(2049), vec![512, 2047]);
    }

    #[test]
    fn tiny_contexts_shrink_and_say_why() {
        let p = derive(256, 1, None, &params());
        assert_eq!(p.prefill, vec![254]);
        assert!(p.decode.is_empty());
        assert!(p.concurrent.is_empty());
        assert!(p.mixed.is_none());
        assert!(p.needle_tokens.is_none());
        let notes = p.notes.join("\n");
        for phase in ["decode:", "concurrent:", "mixed:", "needle:"] {
            assert!(notes.contains(phase), "{notes}");
        }
        assert!(derive(2, 1, None, &params()).prefill.is_empty());
        // 64 + G must fit below S: S = 321 → only D_max = 64.
        assert_eq!(decode_points(321, 256), vec![64]);
        assert_eq!(decode_points(320, 256), vec![63]);
        assert!(decode_points(257, 256).is_empty());
    }

    #[test]
    fn one_slot_has_no_mixed_phase_and_one_concurrent_point() {
        let p = derive(8192, 1, None, &params());
        assert_eq!(p.concurrent, vec![1]);
        assert!(p.mixed.is_none());
        assert!(p.notes.iter().any(|n| n.starts_with("mixed:")));
    }

    #[test]
    fn slot_counts_double_up_to_the_real_count() {
        assert_eq!(concurrent_points(4), vec![1, 2, 4]);
        assert_eq!(concurrent_points(6), vec![1, 2, 4, 6]);
        assert_eq!(concurrent_points(0), Vec::<u32>::new());
    }

    #[test]
    fn a_small_context_injects_its_whole_prefill_top() {
        let p = derive(4096, 4, None, &params());
        let m = p.mixed.unwrap();
        assert_eq!((m.streams, m.inject_tokens), (3, 4094));
        assert_eq!(p.repetitions, 3);
        assert_eq!(p.generate_tokens, 256);
    }

    /// Review finding 4: an auto row (4 unified slots) with a pool of
    /// 12 288 tokens. Before, the three decoding streams and an injection of
    /// min(8192, P_max) could not fit together, llama-server aborted every
    /// running slot, and the phase failed.
    #[test]
    fn a_shared_pool_sizes_the_injection_beside_the_streams() {
        let pool = 12_288;
        let p = derive(pool, 4, Some(pool), &params());
        // One request at a time may use the whole pool.
        assert_eq!(p.prefill.last(), Some(&(pool - 2)));
        assert_eq!(p.decode.last(), Some(&(pool - 257)));
        assert_eq!(p.concurrent, vec![1, 2, 4], "4 × 512 fits");
        let planned = p.mixed.clone().unwrap();
        assert_eq!(planned.inject_tokens, 8192, "the bound before any decoding");
        assert!(p
            .notes
            .iter()
            .any(|n| n.starts_with("mixed:") && n.contains("sized when")));
        // 500 tok/s decode, 20k tok/s prefill: 2 s steady makes ~1000
        // tokens per stream.
        let rates = Rates {
            decode: 500.0,
            prefill: 20_000.0,
        };
        let MixedSizing::Run(sized, Some(note)) =
            size_mixed(&planned, pool, Some(pool), 2000, rates)
        else {
            panic!("fits")
        };
        let streams = u64::from(sized.streams);
        let held =
            streams * (sized.stream_prompt_tokens + sized.stream_predict) + sized.inject_tokens + 1;
        assert!(held <= pool, "{sized:?}: {held} > {pool}");
        assert!(
            sized.inject_tokens < 8192 && sized.inject_tokens > 7000,
            "{sized:?}"
        );
        // The streams decode through the steady window and the injection.
        let needed = (500.0
            * (3.0 * 256.0 / 20_000.0 + 2.0 + sized.inject_tokens as f64 / 20_000.0))
            .ceil() as u64;
        assert!(sized.stream_predict >= needed, "{sized:?}");
        assert!(
            note.contains("12288") && note.contains("the cap is 8192"),
            "{note}"
        );
        // A pool that cannot hold even the streams: a planned skip.
        let MixedSizing::Skip(why) = size_mixed(&planned, 3000, Some(3000), 2000, rates) else {
            panic!("cannot fit")
        };
        assert!(
            why.starts_with("mixed: skipped") && why.contains("3000"),
            "{why}"
        );
        // A split pool of the same size injects the full cap.
        let split = derive(pool, 4, None, &params()).mixed.unwrap();
        assert_eq!(
            size_mixed(&split, pool, None, 2000, rates),
            MixedSizing::Run(split.clone(), None)
        );
    }

    /// Finding 4's other half: a split slot too small to keep a stream
    /// decoding through the 2 s steady window made every repetition fail
    /// "ended during the steady window". Now it is a planned skip.
    #[test]
    fn a_slot_that_cannot_decode_through_the_steady_window_is_skipped() {
        let s = 1280;
        let p = derive(s, 2, None, &params());
        let planned = p.mixed.unwrap();
        assert_eq!(planned.stream_predict, s - 257);
        let rates = Rates {
            decode: 500.0,
            prefill: 20_000.0,
        };
        let MixedSizing::Skip(why) = size_mixed(&planned, s, None, 2000, rates) else {
            panic!("1023 tokens do not last 2 s at 500 tok/s")
        };
        assert!(why.contains("1023") && why.contains("2000 ms"), "{why}");
        // A slower decode lasts.
        let slow = Rates {
            decode: 200.0,
            ..rates
        };
        assert!(matches!(
            size_mixed(&planned, s, None, 2000, slow),
            MixedSizing::Run(_, None)
        ));
    }

    #[test]
    fn a_shared_pool_drops_concurrent_points_that_do_not_fit() {
        // 2048 cells hold 4 streams of 256 + 256 exactly; 1536 hold 3.
        assert_eq!(
            derive(2048, 4, Some(2048), &params()).concurrent,
            vec![1, 2, 4]
        );
        let p = derive(1536, 4, Some(1536), &params());
        assert_eq!(p.concurrent, vec![1, 2]);
        assert!(p
            .notes
            .iter()
            .any(|n| n.starts_with("concurrent:") && n.contains("3 streams")));
        // One slot shares nothing.
        assert_eq!(derive(1536, 1, Some(1536), &params()).concurrent, vec![1]);
    }
}
