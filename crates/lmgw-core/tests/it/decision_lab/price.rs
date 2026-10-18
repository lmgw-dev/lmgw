//! The model list's price rows (labs-store design §3.6): the rows the
//! gateway's own lookup prices a request by, per unit, for the alias's
//! primary route, and a local row's free flag.

use lmgw_api_types::decision_lab::DecisionLabModels;
use lmgw_core::config::{PriceScope, PriceUnit};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::store;

use super::*;

async fn price(
    l: &Lab,
    scope: PriceScope,
    key: &str,
    unit: PriceUnit,
    source: PriceSource,
    tokens: Option<(f64, f64)>,
    rate: Option<f64>,
) {
    let p = Prices {
        price_in: tokens.map(|t| t.0),
        price_out: tokens.map(|t| t.1),
        source,
        ..Default::default()
    };
    store::upsert_price(&l.w.state.db, scope, key, unit, &p, rate, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn each_model_names_the_rows_its_requests_are_priced_by() {
    let l = lab().await;
    let _up = with_jev(&l).await;
    let up = l.w.state.snapshot().resolve("jev").unwrap().upstream.id;
    let up_key = format!("{up}:jev");
    // Tokens: the alias's manual row beats the upstream model's catalog row.
    price(
        &l,
        PriceScope::Alias,
        "jev",
        PriceUnit::PerMtok,
        PriceSource::Manual,
        Some((0.8, 2.4)),
        None,
    )
    .await;
    price(
        &l,
        PriceScope::UpstreamModel,
        &up_key,
        PriceUnit::PerMtok,
        PriceSource::Catalog,
        Some((5.0, 10.0)),
        None,
    )
    .await;
    // A request fee only the upstream model has: per unit, it falls through.
    price(
        &l,
        PriceScope::UpstreamModel,
        &up_key,
        PriceUnit::PerRequest,
        PriceSource::Catalog,
        None,
        Some(0.001),
    )
    .await;
    // A cleared manual row is no row: the catalog's stays.
    price(
        &l,
        PriceScope::Alias,
        "jev",
        PriceUnit::PerRequest,
        PriceSource::Manual,
        None,
        None,
    )
    .await;
    // Another model's row, and a row on the local decision row's name: a
    // local row is free whatever rows exist.
    price(
        &l,
        PriceScope::Alias,
        "other",
        PriceUnit::PerMtok,
        PriceSource::Manual,
        Some((9.0, 9.0)),
        None,
    )
    .await;
    let laya = l.w.name(CPU_ROW);
    price(
        &l,
        PriceScope::Alias,
        &laya,
        PriceUnit::PerMtok,
        PriceSource::Manual,
        Some((1.0, 1.0)),
        None,
    )
    .await;
    l.w.state.reload_snapshot().await.unwrap();

    let resp = l
        .gw()
        .client()
        .get(format!("{}/decision-lab/api/models", l.gw()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let list: DecisionLabModels = resp.json().await.unwrap();
    assert_eq!(list.currency, l.w.state.snapshot().settings.currency);

    let jev = list.models.iter().find(|m| m.name == "jev").unwrap();
    assert!(!jev.local && !jev.price.local, "{jev:?}");
    assert_eq!(jev.price.unresolved, None);
    let rows: Vec<(&str, &str, &str, &str)> = jev
        .price
        .rows
        .iter()
        .map(|r| {
            (
                r.unit.as_str(),
                r.scope_kind.as_str(),
                r.scope_key.as_str(),
                r.source.as_str(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("per_mtok", "alias", "jev", "manual"),
            ("per_request", "upstream_model", up_key.as_str(), "catalog"),
        ]
    );
    assert_eq!(
        (jev.price.rows[0].price_in, jev.price.rows[1].price),
        (Some(0.8), Some(0.001))
    );

    let local = list.models.iter().find(|m| m.name == laya).unwrap();
    assert!(
        local.price.local && local.price.rows.is_empty(),
        "{local:?}"
    );

    // The price the list names is the price a request is recorded at.
    let snap = l.w.state.snapshot();
    let sheet = snap.sheet_for("jev", Some(up), Some("jev"));
    assert_eq!(sheet.tokens.unwrap().price_in, Some(0.8));
    assert_eq!(sheet.request.map(|r| r.price), Some(0.001));
}

#[tokio::test]
async fn a_held_row_names_the_price_of_the_fallback_that_answers_for_it() {
    let l = lab().await;
    let _up = with_jev(&l).await;
    price(
        &l,
        PriceScope::Alias,
        "jev",
        PriceUnit::PerMtok,
        PriceSource::Manual,
        Some((0.8, 2.4)),
        None,
    )
    .await;
    l.w.state.reload_snapshot().await.unwrap();
    let fallen = l.w.name("laya-jev");
    let list = |l: &Lab| {
        let url = format!("{}/decision-lab/api/models", l.gw());
        let client = l.gw().client();
        async move {
            client
                .get(url)
                .send()
                .await
                .unwrap()
                .json::<DecisionLabModels>()
                .await
                .unwrap()
        }
    };

    // Not held: the row is served here, with no fallback in the price.
    let before = list(&l).await;
    let m = before.models.iter().find(|m| m.name == fallen).unwrap();
    assert!(
        m.price.local && !m.held && m.price.fallback.is_none(),
        "{m:?}"
    );

    // Held: every request is answered by `jev`, at its rows.
    lmgw_core::ops::hold_set(&l.w.state, true).await.unwrap();
    let held = list(&l).await;
    let m = held.models.iter().find(|m| m.name == fallen).unwrap();
    assert!(m.held && m.price.local, "{m:?}");
    let f = m.price.fallback.as_ref().expect("the fallback's price");
    assert_eq!(
        (f.alias.as_str(), f.local, f.unresolved.as_deref()),
        ("jev", false, None)
    );
    assert_eq!(f.rows.len(), 1, "{f:?}");
    assert_eq!(
        (
            f.rows[0].unit.as_str(),
            f.rows[0].price_in,
            f.rows[0].price_out
        ),
        ("per_mtok", Some(0.8), Some(2.4))
    );

    // A held row with no fallback has none to name.
    let plain = held
        .models
        .iter()
        .find(|m| m.name == l.w.name(DECISION))
        .unwrap();
    assert!(plain.price.fallback.is_none(), "{plain:?}");
}
