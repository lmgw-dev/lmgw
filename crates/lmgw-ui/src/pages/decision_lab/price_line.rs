//! What a set run on its model costs (labs-store design §3.3, §3.6): before
//! a run the new run shows the rows its requests are priced by, as the
//! server names them on the model (`DecisionLabModel.price`, the rows the
//! gateway's own lookup selects for the alias's primary route, one per
//! unit, each unit resolved on its own: the alias's manual row, the alias's
//! catalog row, the upstream model's manual row, then its catalog row). A
//! local model is free (Usage records its compute, not a price), unless it
//! is held and a fallback answers every item: then the line says so and
//! shows the fallback's rows (`DecisionPrice.fallback`); a cloud
//! model no row covers says Usage records its cost as unknown. The model
//! list is read again when the modal opens on a cloud model, since the owner
//! may have priced it a moment ago; a read that fails says so with Retry.

use leptos::prelude::*;
use leptos_router::components::A;
use lmgw_api_types::decision_lab::{DecisionLabModels, DecisionPrice};
use lmgw_api_types::{PriceRowView, PriceUnit};

use super::page::Lab;
use crate::api;
use crate::fmt::price;

/// One row as the line reads it: "Tokens per 1M tokens: in 0.8 · out 2.4
/// USD (manual, alias)".
pub(super) fn row_text(r: &PriceRowView, currency: &str) -> String {
    let rates = match PriceUnit::parse(&r.unit) {
        Some(u) if u.is_tokens() => {
            let mut parts = Vec::new();
            if let Some(v) = r.price_in {
                parts.push(format!("in {}", price(v)));
            }
            if let Some(v) = r.price_out {
                parts.push(format!("out {}", price(v)));
            }
            if let Some(v) = r.price_cache_read {
                parts.push(format!("cache read {}", price(v)));
            }
            if let Some(v) = r.price_cache_write {
                parts.push(format!("cache write {}", price(v)));
            }
            format!("{} {}: {}", u.label(), u.scale(), parts.join(" · "))
        }
        Some(u) => format!(
            "{} {}: {}",
            u.label(),
            u.scale(),
            r.price.map(price).unwrap_or_else(|| "—".into())
        ),
        None => format!(
            "{}: {}",
            r.unit,
            r.price.map(price).unwrap_or_else(|| "—".into())
        ),
    };
    let scope = if r.scope_kind == "alias" {
        "alias".to_string()
    } else {
        format!("upstream model {}", r.scope_key)
    };
    format!("{rates} {currency} ({}, {scope})", r.source)
}

/// What the line says of `model`'s price.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum PriceSays {
    Free,
    /// The rows, each the line of its unit.
    Priced(Vec<String>),
    /// No row covers it.
    Unpriced,
    /// Its name has no one route (a candidate alias): why.
    Unresolved(String),
    /// The row is held and `alias` answers every item, priced as `says`.
    Fallback {
        alias: String,
        says: Box<PriceSays>,
    },
}

fn rows_say(
    local: bool,
    unresolved: &Option<String>,
    rows: &[PriceRowView],
    currency: &str,
) -> PriceSays {
    if local {
        return PriceSays::Free;
    }
    if let Some(why) = unresolved {
        return PriceSays::Unresolved(why.clone());
    }
    if rows.is_empty() {
        return PriceSays::Unpriced;
    }
    PriceSays::Priced(rows.iter().map(|r| row_text(r, currency)).collect())
}

pub(super) fn price_says(p: &DecisionPrice, currency: &str) -> PriceSays {
    if let Some(f) = &p.fallback {
        return PriceSays::Fallback {
            alias: f.alias.clone(),
            says: Box::new(rows_say(f.local, &f.unresolved, &f.rows, currency)),
        };
    }
    rows_say(p.local, &p.unresolved, &p.rows, currency)
}

/// The line of a held row whose fallback `alias` answers every item.
fn fallback_view(model: &str, alias: &str, says: PriceSays) -> AnyView {
    match says {
        PriceSays::Priced(lines) => {
            let lines = lines
                .into_iter()
                .map(|l| view! { <li class="mono-sm">{l}</li> })
                .collect_view();
            view! {
                <div class="dl-price notice">
                    <b>{format!("{model} is held: {alias} answers every item, one request each.")}</b>
                    <ul class="dl-price-rows">{lines}</ul>
                    <span class="mini-note dim">
                        "Priced by the fallback's own rows; Usage records the real cost. "
                        <A href="/usage/prices">"Prices"</A>
                    </span>
                </div>
            }
            .into_any()
        }
        PriceSays::Free | PriceSays::Fallback { .. } => view! {
            <p class="mini-note dim dl-price">
                {format!("{model} is held and {alias} answers every item: free. Usage records the compute it takes, not a price.")}
            </p>
        }
        .into_any(),
        PriceSays::Unpriced => view! {
            <p class="mini-note dl-warn-line dl-price">
                {format!("{model} is held and {alias} answers every item; no price row covers {alias}: Usage records its cost as unknown. ")}
                <A href="/usage/prices">"Prices"</A>
            </p>
        }
        .into_any(),
        PriceSays::Unresolved(why) => view! {
            <p class="mini-note dl-warn-line dl-price">
                {format!("{model} is held and {alias} answers every item; the price of {alias} is not known before a request: {why}. ")}
                <A href="/usage/prices">"Prices"</A>
            </p>
        }
        .into_any(),
    }
}

#[component]
pub(super) fn PriceLine(lab: Lab) -> impl IntoView {
    let nr = lab.new_run;
    let read = RwSignal::new(None::<Result<DecisionLabModels, String>>);
    let fetch = move || {
        read.set(None);
        lab.scope.spawn(async move {
            let got = api::get::<DecisionLabModels>("/decision-lab/api/models").await;
            read.set(Some(got.map_err(|e| e.to_string())));
        });
    };
    Effect::new(move |_| {
        if !nr.open.get() {
            return;
        }
        // A local row is free, unless held with a fallback answering.
        let free = lab
            .model_of(&nr.model.get())
            .is_none_or(|m| m.local && m.price.fallback.is_none());
        if !free {
            fetch();
        }
    });
    move || {
        let model = nr.model.get();
        if lab
            .model_of(&model)
            .is_some_and(|m| (m.local || m.price.local) && m.price.fallback.is_none())
        {
            return view! {
                <p class="mini-note dim dl-price">
                    {format!("{model} is a local row: free. Usage records the compute it takes, not a price.")}
                </p>
            }
            .into_any();
        }
        let says = match read.get() {
            None => {
                return view! { <p class="mini-note dim dl-price">"Reading the price rows…"</p> }
                    .into_any()
            }
            Some(Err(e)) => {
                return view! {
                    <p class="mini-note bad dl-price">
                        {format!("The price rows did not load: {e} ")}
                        <button class="btn ghost sm" on:click=move |_| fetch()>"Retry"</button>
                    </p>
                }
                .into_any()
            }
            Some(Ok(list)) => match list.models.iter().find(|m| m.name == model) {
                Some(m) => price_says(&m.price, &list.currency),
                None => PriceSays::Unresolved(format!("{model} no longer answers /v1/systemone")),
            },
        };
        match says {
            PriceSays::Fallback { alias, says } => fallback_view(&model, &alias, *says),
            PriceSays::Free => view! {
                <p class="mini-note dim dl-price">
                    {format!("{model} is served on this machine: free. Usage records the compute it takes, not a price.")}
                </p>
            }
            .into_any(),
            PriceSays::Unpriced => view! {
                <p class="mini-note dl-warn-line dl-price">
                    {format!("No price row covers {model}: Usage records its cost as unknown. ")}
                    <A href="/usage/prices">"Prices"</A>
                </p>
            }
            .into_any(),
            PriceSays::Unresolved(why) => view! {
                <p class="mini-note dl-warn-line dl-price">
                    {format!("The price of {model} is not known before a request: {why}. ")}
                    <A href="/usage/prices">"Prices"</A>
                </p>
            }
            .into_any(),
            PriceSays::Priced(lines) => {
                let lines = lines
                    .into_iter()
                    .map(|l| view! { <li class="mono-sm">{l}</li> })
                    .collect_view();
                view! {
                    <div class="dl-price notice">
                        <b>{format!("{model} is priced: each item is one request.")}</b>
                        <ul class="dl-price-rows">{lines}</ul>
                        <span class="mini-note dim">
                            "Each unit is priced by its own row, as the gateway prices the request (a fallback that answers is priced by its own rows); Usage records the real cost. "
                            <A href="/usage/prices">"Prices"</A>
                        </span>
                    </div>
                }
                .into_any()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, key: &str, unit: &str, source: &str) -> PriceRowView {
        PriceRowView {
            scope_kind: kind.into(),
            scope_key: key.into(),
            unit: unit.into(),
            source: source.into(),
            price_in: (unit == "per_mtok").then_some(0.8),
            price_out: (unit == "per_mtok").then_some(2.4),
            price: (unit != "per_mtok").then_some(0.001),
            ..Default::default()
        }
    }

    #[test]
    fn a_held_rows_price_is_its_fallbacks() {
        let mut p = DecisionPrice {
            local: true,
            ..Default::default()
        };
        assert_eq!(price_says(&p, "USD"), PriceSays::Free);
        p.fallback = Some(lmgw_api_types::decision_lab::DecisionFallbackPrice {
            alias: "jev".into(),
            rows: vec![row("alias", "jev", "per_mtok", "manual")],
            ..Default::default()
        });
        let PriceSays::Fallback { alias, says } = price_says(&p, "USD") else {
            panic!("the fallback answers")
        };
        assert_eq!(alias, "jev");
        assert!(matches!(*says, PriceSays::Priced(ref l) if l.len() == 1));
    }

    #[test]
    fn a_row_reads_its_rates_and_scope() {
        assert_eq!(
            row_text(&row("alias", "jev", "per_mtok", "manual"), "USD"),
            "Tokens per 1M tokens: in 0.8 · out 2.4 USD (manual, alias)"
        );
        assert_eq!(
            row_text(
                &row("upstream_model", "3:jev", "per_request", "catalog"),
                "USD"
            ),
            "Requests per answered request: 0.001 USD (catalog, upstream model 3:jev)"
        );
    }

    #[test]
    fn the_line_says_what_the_server_names() {
        let local = DecisionPrice {
            local: true,
            ..Default::default()
        };
        assert_eq!(price_says(&local, "USD"), PriceSays::Free);
        assert_eq!(
            price_says(&DecisionPrice::default(), "USD"),
            PriceSays::Unpriced
        );
        // Per unit, each its own row: the alias's tokens, the upstream
        // model's request fee beneath them.
        let priced = DecisionPrice {
            rows: vec![
                row("alias", "jev", "per_mtok", "manual"),
                row("upstream_model", "3:jev", "per_request", "catalog"),
            ],
            ..Default::default()
        };
        match price_says(&priced, "EUR") {
            PriceSays::Priced(lines) => {
                assert_eq!(lines.len(), 2);
                assert!(lines[0].ends_with("EUR (manual, alias)"), "{lines:?}");
                assert!(lines[1].contains("upstream model 3:jev"), "{lines:?}");
            }
            other => panic!("{other:?}"),
        }
        let unresolved = DecisionPrice {
            unresolved: Some("a candidate alias".into()),
            ..Default::default()
        };
        assert_eq!(
            price_says(&unresolved, "USD"),
            PriceSays::Unresolved("a candidate alias".into())
        );
    }
}
