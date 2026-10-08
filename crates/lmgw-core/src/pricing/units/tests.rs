//! Billable units' arithmetic (billable-units design §10): §3.3's goldens,
//! no false zero, the local zero (Q1), and the token oracle.

use super::*;
use crate::pricing::micro_to_units;

fn rate(price: f64) -> Option<UnitRate> {
    Some(UnitRate {
        price,
        source: PriceSource::Catalog,
    })
}

fn manual(price: f64) -> Option<UnitRate> {
    Some(UnitRate {
        price,
        source: PriceSource::Manual,
    })
}

/// 3 / 15 per 1M tokens, no cache rates.
fn claude_ish() -> Prices {
    Prices {
        price_in: Some(3.0),
        price_out: Some(15.0),
        source: PriceSource::Catalog,
        ..Default::default()
    }
}

fn no_tokens() -> TokenUsage {
    TokenUsage::default()
}

// --- §3.3's goldens ---------------------------------------------------------

#[test]
fn whisper_27_seconds_at_0_006_a_minute_is_2700_micro() {
    let sheet = Sheet {
        audio_minute: rate(0.006),
        ..Default::default()
    };
    let q = Quantities {
        audio_in_ms: Some(27_000),
        ..Default::default()
    };
    let c = price_request(&no_tokens(), &q, &sheet);
    assert_eq!(c.total_micro, Some(2_700));
    assert_eq!(c.units_micro, Some(2_700));
    assert_eq!(c.in_micro, None, "no token row, no token part");
    assert_eq!(c.out_micro, None);
    assert_eq!(c.used_units.audio_minute, Some(0.006));
    assert_eq!(c.used_units.mchar, None);
    assert_eq!(c.source, PriceSource::Catalog);
    assert_eq!(micro_to_units(c.total_micro.unwrap()), 0.0027);

    // A reported 27.4 s is 27 400 ms.
    let q = Quantities {
        audio_in_ms: Some(27_400),
        ..Default::default()
    };
    assert_eq!(
        price_request(&no_tokens(), &q, &sheet).total_micro,
        Some(2_740)
    );
}

#[test]
fn tts_characters_at_15_and_30_per_million() {
    let q = Quantities {
        chars_in: Some(1_234),
        ..Default::default()
    };
    for (price, micro) in [(15.0, 18_510), (30.0, 37_020)] {
        let sheet = Sheet {
            mchar: rate(price),
            ..Default::default()
        };
        let c = price_request(&no_tokens(), &q, &sheet);
        assert_eq!(c.total_micro, Some(micro), "{price}");
        assert_eq!(c.units_micro, Some(micro));
        assert_eq!(c.used_units.mchar, Some(price));
    }
}

#[test]
fn two_images_at_four_cents() {
    let sheet = Sheet {
        image: rate(0.04),
        ..Default::default()
    };
    let q = Quantities {
        images_out: Some(2),
        ..Default::default()
    };
    let c = price_request(&no_tokens(), &q, &sheet);
    assert_eq!(c.total_micro, Some(80_000));
    assert_eq!(c.units_micro, Some(80_000));
    assert_eq!(c.used_units.image, Some(0.04));
}

/// Tokens plus a per-request fee, and the same request unanswered.
#[test]
fn tokens_plus_a_request_fee_and_unanswered_is_null() {
    let sheet = Sheet {
        tokens: Some(claude_ish()),
        request: rate(0.005),
        ..Default::default()
    };
    let tokens = TokenUsage {
        prompt: Some(1_000_000),
        completion: Some(100_000),
        ..Default::default()
    };
    let c = price_request(&tokens, &Quantities::answered(), &sheet);
    assert_eq!(c.in_micro, Some(3_000_000));
    assert_eq!(c.out_micro, Some(1_500_000));
    assert_eq!(c.units_micro, Some(5_000));
    assert_eq!(c.total_micro, Some(4_505_000));
    assert_eq!(c.used.price_in, Some(3.0));
    assert_eq!(c.used_units.request, Some(0.005));
    assert_eq!(c.source, PriceSource::Catalog);

    // Not answered: `requests` is `None`, and the whole row is NULL — not
    // the token part alone, which would be a partial sum.
    let c = price_request(&tokens, &Quantities::default(), &sheet);
    assert_eq!(c.total_micro, None);
    assert_eq!(c.in_micro, None);
    assert_eq!(c.out_micro, None);
    assert_eq!(c.units_micro, None);
    assert_eq!(c.source, PriceSource::Unknown);
    // …and the rates that would have applied are still on the row.
    assert_eq!(c.used.price_in, Some(3.0));
    assert_eq!(c.used.price_out, Some(15.0));
    assert_eq!(c.used_units.request, Some(0.005));
}

/// Every part is rounded on its own, then summed as integers (§3.3).
#[test]
fn each_part_rounds_on_its_own() {
    let sheet = Sheet {
        audio_minute: rate(0.006),
        mchar: rate(15.0),
        image: rate(0.04),
        request: rate(0.005),
        ..Default::default()
    };
    let q = Quantities {
        audio_in_ms: Some(1), // 0.1 micro → 0
        chars_in: Some(1),    // 15 micro
        images_out: Some(0),  // a measured 0 is a real 0
        requests: Some(3),    // 15 000 micro
    };
    let c = price_request(&no_tokens(), &q, &sheet);
    assert_eq!(c.units_micro, Some(15 + 15_000));
    assert_eq!(c.total_micro, Some(15_015));
}

// --- No false zero ----------------------------------------------------------

#[test]
fn a_priced_unit_without_its_quantity_is_null_and_keeps_its_rate() {
    for (unit, sheet) in [
        (
            PriceUnit::PerAudioMinute,
            Sheet {
                audio_minute: rate(0.006),
                ..Default::default()
            },
        ),
        (
            PriceUnit::PerMchar,
            Sheet {
                mchar: rate(15.0),
                ..Default::default()
            },
        ),
        (
            PriceUnit::PerImage,
            Sheet {
                image: rate(0.04),
                ..Default::default()
            },
        ),
        (
            PriceUnit::PerRequest,
            Sheet {
                request: rate(0.005),
                ..Default::default()
            },
        ),
    ] {
        let c = price_request(&no_tokens(), &Quantities::default(), &sheet);
        assert_eq!(c.total_micro, None, "{unit}");
        assert_eq!(c.units_micro, None, "{unit}");
        assert_eq!(c.source, PriceSource::Unknown, "{unit}");
        assert_eq!(c.used_units.get(unit), sheet.rate(unit).map(|r| r.price));
    }
}

/// A quantity the sheet does not price is recorded on the row and costs
/// nothing extra; one it does price but did not measure is unknown, even
/// when another unit's quantity is known.
#[test]
fn only_a_priced_unit_needs_its_quantity() {
    let sheet = Sheet {
        mchar: rate(15.0),
        request: rate(0.005),
        ..Default::default()
    };
    let measured = Quantities {
        audio_in_ms: Some(60_000),
        chars_in: Some(1_000),
        requests: None,
        ..Default::default()
    };
    assert_eq!(
        price_request(&no_tokens(), &measured, &sheet).total_micro,
        None
    );
    let answered = Quantities {
        requests: Some(1),
        ..measured
    };
    assert_eq!(
        price_request(&no_tokens(), &answered, &sheet).total_micro,
        Some(15_000 + 5_000),
        "the audio has no rate here"
    );
}

#[test]
fn no_rate_in_any_unit_is_unpriced() {
    let q = Quantities {
        audio_in_ms: Some(1),
        chars_in: Some(1),
        images_out: Some(1),
        requests: Some(1),
    };
    let tokens = TokenUsage {
        prompt: Some(1),
        completion: Some(1),
        ..Default::default()
    };
    for sheet in [Sheet::default(), Sheet::tokens(Some(Prices::default()))] {
        let c = price_request(&tokens, &q, &sheet);
        assert_eq!(c, Cost::unknown(), "{sheet:?}");
        assert!(!sheet.is_priced());
    }
}

/// A token row with no token count stays unknown, even though every other
/// unit's quantity is known (§3.2 rule 3).
#[test]
fn a_token_row_without_a_token_count_stays_null_beside_known_units() {
    let sheet = Sheet {
        tokens: Some(claude_ish()),
        mchar: rate(15.0),
        request: rate(0.005),
        ..Default::default()
    };
    let q = Quantities {
        chars_in: Some(1_234),
        requests: Some(1),
        ..Default::default()
    };
    let c = price_request(&no_tokens(), &q, &sheet);
    assert_eq!(c.total_micro, None);
    assert_eq!(c.units_micro, None, "not the units part alone");
    assert_eq!(c.used.price_in, Some(3.0));
    assert_eq!(c.used_units.mchar, Some(15.0));
    assert_eq!(c.used_units.request, Some(0.005));
}

// --- Local (Q1) -------------------------------------------------------------

#[test]
fn a_local_sheet_is_a_real_zero_with_or_without_tokens() {
    let q = Quantities {
        audio_in_ms: Some(27_000),
        chars_in: None,
        images_out: Some(2),
        requests: None,
    };
    for tokens in [
        no_tokens(),
        TokenUsage {
            prompt: Some(5_000_000),
            completion: Some(1_000_000),
            ..Default::default()
        },
    ] {
        let c = price_request(&tokens, &q, &Sheet::local());
        assert_eq!(c.total_micro, Some(0), "{tokens:?}");
        assert_eq!(c.in_micro, Some(0));
        assert_eq!(c.out_micro, Some(0));
        assert_eq!(c.units_micro, None, "a local sheet has no unit rows");
        assert_eq!(c.source, PriceSource::FreeLocal);
        assert_eq!(c.used, Prices::free_local());
        assert_eq!(c.used_units, UnitRates::default());
    }
    assert!(Sheet::local().is_priced());
}

/// With tokens, the local zero is exactly what it was before units.
#[test]
fn a_local_sheet_with_tokens_matches_the_token_function() {
    let tokens = TokenUsage {
        prompt: Some(10),
        completion: Some(20),
        cached_in: Some(5),
        ..Default::default()
    };
    assert_eq!(
        price_request(&tokens, &Quantities::default(), &Sheet::local()),
        price_tokens(&tokens, Some(&Prices::free_local()))
    );
}

// --- Source -----------------------------------------------------------------

#[test]
fn the_source_is_manual_if_any_rate_used_was() {
    let tokens = TokenUsage {
        prompt: Some(1),
        completion: Some(1),
        ..Default::default()
    };
    let q = Quantities::answered();
    let catalog_tokens = Some(claude_ish());
    let manual_tokens = Some(Prices {
        source: PriceSource::Manual,
        ..claude_ish()
    });
    let cases = [
        (catalog_tokens, rate(0.01), PriceSource::Catalog),
        (catalog_tokens, manual(0.01), PriceSource::Manual),
        (manual_tokens, rate(0.01), PriceSource::Manual),
        (None, rate(0.01), PriceSource::Catalog),
        (None, manual(0.01), PriceSource::Manual),
    ];
    for (tok, req, want) in cases {
        let sheet = Sheet {
            tokens: tok,
            request: req,
            ..Default::default()
        };
        assert_eq!(price_request(&tokens, &q, &sheet).source, want, "{sheet:?}");
    }
}

/// A manual 0 is how the owner drops a fee (§2.3): a real 0, priced.
#[test]
fn a_manual_zero_rate_is_a_real_zero() {
    let sheet = Sheet {
        request: manual(0.0),
        ..Default::default()
    };
    let c = price_request(&no_tokens(), &Quantities::answered(), &sheet);
    assert_eq!(c.total_micro, Some(0));
    assert_eq!(c.units_micro, Some(0));
    assert_eq!(c.source, PriceSource::Manual);
}

/// 0 × anything is a known 0: a fee dropped with a manual 0 leaves a row
/// whose request count is unknown — a stop before the answer — priced on
/// its tokens, not unpriced (review WP2 #4). A non-zero rate still needs
/// its quantity.
#[test]
fn a_zero_rate_prices_an_unknown_quantity_at_a_known_zero() {
    let tokens = TokenUsage {
        prompt: Some(1_000_000),
        completion: Some(100_000),
        ..Default::default()
    };
    let stopped_before_the_answer = Quantities::default();
    let dropped = Sheet {
        tokens: Some(claude_ish()),
        request: manual(0.0),
        ..Default::default()
    };
    let c = price_request(&tokens, &stopped_before_the_answer, &dropped);
    assert_eq!(c.total_micro, Some(4_500_000));
    assert_eq!(c.units_micro, Some(0));
    assert_eq!(c.used_units.request, Some(0.0));
    assert_eq!(c.source, PriceSource::Manual);

    let every_unit_free = Sheet {
        audio_minute: manual(0.0),
        mchar: manual(0.0),
        image: manual(0.0),
        request: manual(0.0),
        ..Default::default()
    };
    let c = price_request(&no_tokens(), &Quantities::default(), &every_unit_free);
    assert_eq!(c.total_micro, Some(0));

    let charged = Sheet {
        request: manual(0.005),
        ..dropped
    };
    let c = price_request(&tokens, &stopped_before_the_answer, &charged);
    assert_eq!(c.total_micro, None);
}

// --- Quantities -------------------------------------------------------------

#[test]
fn reported_quantities_win_over_measured_ones() {
    let reported = Quantities {
        audio_in_ms: Some(27_400),
        ..Default::default()
    };
    let measured = Quantities {
        audio_in_ms: Some(27_391),
        chars_in: Some(12),
        requests: Some(1),
        ..Default::default()
    };
    assert_eq!(
        reported.over(measured),
        Quantities {
            audio_in_ms: Some(27_400),
            chars_in: Some(12),
            images_out: None,
            requests: Some(1),
        }
    );
    assert_eq!(Quantities::answered().requests, Some(1));
    assert_eq!(Quantities::answered().audio_in_ms, None);
}

// --- The token oracle -------------------------------------------------------

/// `price_request` as it was before billable units, kept verbatim as the
/// oracle (decision 1): `(total, in, out, source, used)`.
fn today(
    usage: &TokenUsage,
    prices: Option<&Prices>,
) -> (Option<i64>, Option<i64>, Option<i64>, PriceSource, Prices) {
    let Some(p) = prices.filter(|p| p.is_usable()) else {
        return (None, None, None, PriceSource::Unknown, Prices::default());
    };
    if usage.prompt.is_none() && usage.completion.is_none() {
        return (None, None, None, PriceSource::Unknown, *p);
    }
    let rate_in = p.price_in.unwrap_or(0.0);
    let rate_cache_read = p.price_cache_read.unwrap_or(rate_in);
    let rate_cache_write = p.price_cache_write.unwrap_or(rate_in);
    let rate_out = p.price_out.unwrap_or(0.0);

    let in_micro = (usage.plain_in() as f64) * rate_in
        + (usage.cached_in.unwrap_or(0) as f64) * rate_cache_read
        + (usage.cache_write.unwrap_or(0) as f64) * rate_cache_write;
    let out_micro = (usage.completion.unwrap_or(0) as f64) * rate_out;

    let in_micro = in_micro.round() as i64;
    let out_micro = out_micro.round() as i64;
    (
        Some(in_micro + out_micro),
        Some(in_micro),
        Some(out_micro),
        p.source,
        Prices {
            price_in: Some(rate_in),
            price_out: Some(rate_out),
            price_cache_read: Some(rate_cache_read),
            price_cache_write: Some(rate_cache_write),
            source: p.source,
        },
    )
}

/// For a grid of token counts (`None`/`Some` on every field, cache counts
/// above the total) and token-only sheets (with and without cache rates,
/// catalog and manual, unusable ones included), the new function given any
/// quantities equals the oracle field for field (§10).
#[test]
fn a_token_only_sheet_prices_exactly_as_before_whatever_the_quantities() {
    let counts = |vals: &[u64]| -> Vec<Option<u64>> {
        std::iter::once(None)
            .chain(vals.iter().copied().map(Some))
            .collect()
    };
    let prompts = counts(&[0, 1_000, 1_234_567]);
    let completions = counts(&[0, 777, 2_000_000]);
    let cached = counts(&[0, 500, 5_000_000]);
    let writes = counts(&[0, 300, 9_999_999]);
    let reasoning = counts(&[0, 100]);

    let mut sheets: Vec<Option<Prices>> = vec![None];
    for price_in in [None, Some(3.0), Some(0.15)] {
        for price_out in [None, Some(15.0), Some(0.6)] {
            for price_cache_read in [None, Some(0.3)] {
                for price_cache_write in [None, Some(3.75)] {
                    for source in [
                        PriceSource::Catalog,
                        PriceSource::Manual,
                        // No stored row carries either, and a token-only
                        // sheet on a non-local scope still prices them as
                        // the token function always did.
                        PriceSource::FreeLocal,
                        PriceSource::Unknown,
                    ] {
                        sheets.push(Some(Prices {
                            price_in,
                            price_out,
                            price_cache_read,
                            price_cache_write,
                            source,
                        }));
                    }
                }
            }
        }
    }
    let quantities = [
        Quantities::default(),
        Quantities {
            audio_in_ms: Some(27_000),
            chars_in: Some(1_234),
            images_out: Some(2),
            requests: Some(1),
        },
        Quantities {
            audio_in_ms: None,
            chars_in: Some(0),
            images_out: None,
            requests: Some(7),
        },
    ];

    let mut checked = 0u64;
    for &prompt in &prompts {
        for &completion in &completions {
            for &cached_in in &cached {
                for &cache_write in &writes {
                    for &r in &reasoning {
                        let usage = TokenUsage {
                            prompt,
                            completion,
                            cached_in,
                            cache_write,
                            reasoning: r,
                        };
                        for p in &sheets {
                            let (total, inn, out, source, used) = today(&usage, p.as_ref());
                            let sheet = Sheet::tokens(*p);
                            for q in &quantities {
                                let c = price_request(&usage, q, &sheet);
                                let ctx = || format!("{usage:?} {p:?} {q:?}");
                                assert_eq!(c.total_micro, total, "{}", ctx());
                                assert_eq!(c.in_micro, inn, "{}", ctx());
                                assert_eq!(c.out_micro, out, "{}", ctx());
                                assert_eq!(c.source, source, "{}", ctx());
                                assert_eq!(c.used, used, "{}", ctx());
                                assert_eq!(c.units_micro, None, "{}", ctx());
                                assert_eq!(c.used_units, UnitRates::default(), "{}", ctx());
                                checked += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(checked > 100_000, "{checked}");
}

/// The token function itself still is the oracle, field for field.
#[test]
fn the_token_part_is_todays_function() {
    let usage = TokenUsage {
        prompt: Some(1_000_000),
        completion: Some(10),
        cached_in: Some(800_000),
        cache_write: Some(100_000),
        reasoning: None,
    };
    for p in [None, Some(claude_ish()), Some(Prices::default())] {
        let c = price_tokens(&usage, p.as_ref());
        let (total, inn, out, source, used) = today(&usage, p.as_ref());
        assert_eq!(
            (c.total_micro, c.in_micro, c.out_micro, c.source, c.used),
            (total, inn, out, source, used)
        );
    }
}
