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
    s.sheet_for("asr", Some(UP), Some(MODEL))
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
    let sh = s.sheet_for("asr", Some(AUDIO_UPSTREAM_ID), Some(MODEL));
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
    assert!(s.sheet_for("asr", None, None).request.is_some());
}
