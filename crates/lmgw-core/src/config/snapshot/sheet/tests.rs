//! `sheet_for` (billable-units design §2.3, §10): each unit resolves on its
//! own through the four-step chain, a cleared row never shadows, and a local
//! upstream is free whatever rows exist.

use super::*;

const UP: i64 = 7;
const MODEL: &str = "whisper-1";

fn row(scope: PriceScope, key: &str, source: PriceSource, unit: PriceUnit) -> PriceRow {
    PriceRow {
        id: 0,
        scope_kind: scope,
        scope_key: key.into(),
        unit,
        price_in: None,
        price_out: None,
        price_cache_read: None,
        price_cache_write: None,
        source,
        note: None,
        updated_at: String::new(),
        price: None,
        service_tier: PriceTier::Standard,
    }
}

fn tokens(scope: PriceScope, key: &str, source: PriceSource, price_in: f64) -> PriceRow {
    PriceRow {
        price_in: Some(price_in),
        price_out: Some(price_in * 5.0),
        ..row(scope, key, source, PriceUnit::PerMtok)
    }
}

fn unit(
    scope: PriceScope,
    key: &str,
    source: PriceSource,
    unit: PriceUnit,
    price: Option<f64>,
) -> PriceRow {
    PriceRow {
        price,
        ..row(scope, key, source, unit)
    }
}

fn snap(prices: Vec<PriceRow>) -> Snapshot {
    Snapshot {
        prices,
        ..Default::default()
    }
}

fn up_key() -> String {
    upstream_scope_key(UP, MODEL)
}

fn sheet(s: &Snapshot) -> Sheet {
    s.sheet_for("asr", Some(UP), Some(MODEL), Some(PriceTier::Standard))
}

fn price_of(s: &Sheet, u: PriceUnit) -> Option<(f64, PriceSource)> {
    s.rate(u).map(|r| (r.price, r.source))
}

#[test]
fn manual_beats_catalog_per_unit() {
    let s = snap(vec![
        unit(
            PriceScope::Alias,
            "asr",
            PriceSource::Catalog,
            PriceUnit::PerAudioMinute,
            Some(0.006),
        ),
        unit(
            PriceScope::Alias,
            "asr",
            PriceSource::Manual,
            PriceUnit::PerAudioMinute,
            Some(0.005),
        ),
        unit(
            PriceScope::Alias,
            "asr",
            PriceSource::Catalog,
            PriceUnit::PerRequest,
            Some(0.001),
        ),
    ]);
    let sh = sheet(&s);
    assert_eq!(
        price_of(&sh, PriceUnit::PerAudioMinute),
        Some((0.005, PriceSource::Manual))
    );
    assert_eq!(
        price_of(&sh, PriceUnit::PerRequest),
        Some((0.001, PriceSource::Catalog)),
        "the catalog's fee still applies: no manual row in that unit"
    );
    assert_eq!(sh.tokens, None);
    assert!(sh.is_priced());
}

#[test]
fn the_alias_beats_the_upstream_model_per_unit() {
    let s = snap(vec![
        unit(
            PriceScope::UpstreamModel,
            &up_key(),
            PriceSource::Manual,
            PriceUnit::PerMchar,
            Some(30.0),
        ),
        unit(
            PriceScope::Alias,
            "asr",
            PriceSource::Catalog,
            PriceUnit::PerMchar,
            Some(15.0),
        ),
        unit(
            PriceScope::UpstreamModel,
            &up_key(),
            PriceSource::Catalog,
            PriceUnit::PerImage,
            Some(0.04),
        ),
    ]);
    let sh = sheet(&s);
    assert_eq!(
        price_of(&sh, PriceUnit::PerMchar),
        Some((15.0, PriceSource::Catalog)),
        "the alias's catalog row beats the model's manual one"
    );
    assert_eq!(
        price_of(&sh, PriceUnit::PerImage),
        Some((0.04, PriceSource::Catalog)),
        "a unit the alias has no row for falls to its upstream model"
    );
}

/// A manual token row together with a catalog request fee: both apply.
#[test]
fn a_manual_token_row_and_a_catalog_request_row_both_apply() {
    let s = snap(vec![
        tokens(PriceScope::Alias, "asr", PriceSource::Catalog, 2.5),
        tokens(PriceScope::Alias, "asr", PriceSource::Manual, 1.0),
        unit(
            PriceScope::UpstreamModel,
            &up_key(),
            PriceSource::Catalog,
            PriceUnit::PerRequest,
            Some(0.005),
        ),
    ]);
    let sh = sheet(&s);
    let t = sh.tokens.unwrap();
    assert_eq!(t.price_in, Some(1.0));
    assert_eq!(t.source, PriceSource::Manual);
    assert_eq!(
        price_of(&sh, PriceUnit::PerRequest),
        Some((0.005, PriceSource::Catalog))
    );
    let c = crate::pricing::price_request(
        &crate::pricing::TokenUsage {
            prompt: Some(1_000_000),
            completion: Some(0),
            ..Default::default()
        },
        &crate::pricing::Quantities::answered(),
        &sh,
    );
    assert_eq!(c.total_micro, Some(1_000_000 + 5_000));
    assert_eq!(c.source, PriceSource::Manual);
}

/// A cleared row — every rate of its unit `None` — is no row, so it never
/// shadows the next step of the chain.
#[test]
fn a_cleared_row_never_shadows() {
    let s = snap(vec![
        unit(
            PriceScope::Alias,
            "asr",
            PriceSource::Manual,
            PriceUnit::PerAudioMinute,
            None,
        ),
        unit(
            PriceScope::Alias,
            "asr",
            PriceSource::Catalog,
            PriceUnit::PerAudioMinute,
            Some(0.006),
        ),
        row(
            PriceScope::Alias,
            "asr",
            PriceSource::Manual,
            PriceUnit::PerMtok,
        ),
        tokens(
            PriceScope::UpstreamModel,
            &up_key(),
            PriceSource::Catalog,
            2.5,
        ),
    ]);
    let sh = sheet(&s);
    assert_eq!(
        price_of(&sh, PriceUnit::PerAudioMinute),
        Some((0.006, PriceSource::Catalog))
    );
    assert_eq!(sh.tokens.unwrap().price_in, Some(2.5));
}

/// A scope with no usable row in any unit prices nothing, and the token
/// view of it is `None`: unknown, not zero.
#[test]
fn no_row_is_unpriced_and_the_token_view_is_none() {
    let s = snap(vec![unit(
        PriceScope::Alias,
        "other",
        PriceSource::Manual,
        PriceUnit::PerRequest,
        Some(1.0),
    )]);
    let sh = sheet(&s);
    assert_eq!(sh, Sheet::default());
    assert!(!sh.is_priced());
    assert_eq!(s.prices_for("asr", Some(UP), Some(MODEL)), None);
}

/// Alias keys match case-insensitively, as they always did.
#[test]
fn alias_keys_match_case_insensitively() {
    let s = snap(vec![unit(
        PriceScope::Alias,
        "ASR",
        PriceSource::Manual,
        PriceUnit::PerRequest,
        Some(1.0),
    )]);
    assert!(sheet(&s).request.is_some());
}

/// A local upstream is free whatever rows exist, and the token view says so
/// as it always did.
#[test]
fn a_local_upstream_is_free_whatever_rows_exist() {
    let s = snap(vec![
        tokens(PriceScope::Alias, "asr", PriceSource::Manual, 2.0),
        unit(
            PriceScope::Alias,
            "asr",
            PriceSource::Manual,
            PriceUnit::PerAudioMinute,
            Some(0.006),
        ),
    ]);
    let sh = s.sheet_for(
        "asr",
        Some(AUDIO_UPSTREAM_ID),
        Some(MODEL),
        Some(PriceTier::Standard),
    );
    assert_eq!(sh, Sheet::local());
    assert_eq!(
        s.prices_for("asr", Some(AUDIO_UPSTREAM_ID), Some(MODEL)),
        Some(Prices::free_local())
    );
}

/// `prices_for` is the token sheet alone, the same rows as before units.
#[test]
fn prices_for_is_the_token_sheet() {
    let s = snap(vec![
        tokens(
            PriceScope::UpstreamModel,
            &up_key(),
            PriceSource::Catalog,
            2.5,
        ),
        unit(
            PriceScope::Alias,
            "asr",
            PriceSource::Manual,
            PriceUnit::PerRequest,
            Some(1.0),
        ),
    ]);
    assert_eq!(s.prices_for("asr", Some(UP), Some(MODEL)), sheet(&s).tokens);
    assert_eq!(
        s.prices_for("asr", Some(UP), Some(MODEL)).unwrap().price_in,
        Some(2.5)
    );
    // Without a route, only the alias tier is consulted.
    assert_eq!(s.prices_for("asr", None, None), None);
    assert!(s
        .sheet_for("asr", None, None, Some(PriceTier::Standard))
        .request
        .is_some());
}

// --- Service tiers (openai egress design §5.5, §5.6) ---------------------

fn at(tier: PriceTier, r: PriceRow) -> PriceRow {
    PriceRow {
        service_tier: tier,
        ..r
    }
}

fn sheet_at(s: &Snapshot, tier: Option<PriceTier>) -> Sheet {
    s.sheet_for("asr", Some(UP), Some(MODEL), tier)
}

/// A flex answer on a scope with Standard rows only: no token sheet, a gap
/// naming Standard, and the request unpriced — never priced at Standard.
#[test]
fn a_tier_with_no_row_is_a_gap_not_standard() {
    let s = snap(vec![tokens(
        PriceScope::UpstreamModel,
        &up_key(),
        PriceSource::Manual,
        4.0,
    )]);
    let sh = sheet_at(&s, Some(PriceTier::Flex));
    assert_eq!(sh.tokens, None);
    let gap = sh.tier_gap.expect("a gap");
    assert_eq!(
        gap.on_file.iter().collect::<Vec<_>>(),
        [PriceTier::Standard]
    );
    let cost = crate::pricing::price_request(
        &crate::pricing::TokenUsage {
            prompt: Some(1000),
            completion: Some(10),
            ..Default::default()
        },
        &crate::pricing::Quantities::answered(),
        &sh,
    );
    assert_eq!(cost.total_micro, None);
    assert_eq!(cost.source, PriceSource::Unknown);

    // Standard is untouched, and has no gap.
    let std = sheet_at(&s, Some(PriceTier::Standard));
    assert_eq!(std.tokens.unwrap().price_in, Some(4.0));
    assert_eq!(std.tier_gap, None);
}

/// Each chain step is restricted to the tier: an alias's Standard row does
/// not shadow the upstream model's flex row for a flex answer, and a flex
/// row never prices a Standard one.
#[test]
fn the_chain_runs_within_the_tier() {
    let s = snap(vec![
        tokens(PriceScope::Alias, "asr", PriceSource::Manual, 4.0),
        at(
            PriceTier::Flex,
            tokens(
                PriceScope::UpstreamModel,
                &up_key(),
                PriceSource::Manual,
                2.0,
            ),
        ),
    ]);
    let flex = sheet_at(&s, Some(PriceTier::Flex));
    assert_eq!(flex.tokens.unwrap().price_in, Some(2.0));
    assert_eq!(flex.tier_gap, None);
    assert_eq!(
        sheet_at(&s, Some(PriceTier::Standard))
            .tokens
            .unwrap()
            .price_in,
        Some(4.0)
    );
    // Fast has neither: a gap naming both tiers on file.
    let fast = sheet_at(&s, Some(PriceTier::Fast));
    assert_eq!(
        fast.tier_gap.unwrap().on_file.iter().collect::<Vec<_>>(),
        [PriceTier::Standard, PriceTier::Flex]
    );
}

/// Per unit: a flex token row with a Standard-only per-request fee still
/// leaves the fee's unit without a flex row, which is a gap.
#[test]
fn a_unit_missing_at_the_tier_is_a_gap_too() {
    let s = snap(vec![
        at(
            PriceTier::Flex,
            tokens(PriceScope::Alias, "asr", PriceSource::Manual, 2.0),
        ),
        unit(
            PriceScope::Alias,
            "asr",
            PriceSource::Manual,
            PriceUnit::PerRequest,
            Some(0.01),
        ),
    ]);
    let sh = sheet_at(&s, Some(PriceTier::Flex));
    assert!(sh.tokens.is_some());
    assert_eq!(sh.request, None);
    assert!(sh.tier_gap.is_some());
}

/// An answer that named no price tier (`auto`, or none at all) is priced by
/// no row: a gap whenever the scope has rows, nothing at all when it has
/// none. A local upstream is free whatever the tier.
#[test]
fn no_tier_prices_nothing() {
    let s = snap(vec![tokens(
        PriceScope::Alias,
        "asr",
        PriceSource::Manual,
        4.0,
    )]);
    let sh = sheet_at(&s, None);
    assert_eq!(sh.tokens, None);
    assert!(sh.tier_gap.is_some());
    assert_eq!(sheet_at(&snap(vec![]), None).tier_gap, None);
    assert_eq!(
        s.sheet_for("asr", Some(AUDIO_UPSTREAM_ID), Some(MODEL), None),
        Sheet::local()
    );
}
