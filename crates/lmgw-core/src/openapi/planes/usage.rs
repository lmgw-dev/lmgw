//! `/api/usage/*` (api-docs design §4.6). WP4.
//!
//! One query surface backs every chart on the Usage page (`web/api_usage.rs`'s
//! own module doc): each read is a thin translation of query params into a
//! `store::UsageFilter` plus a call into the tested aggregation layer, so a
//! chart cannot invent its own arithmetic.

use lmgw_api_types as dto;

use super::super::registry::{Dialect, DocRoute, Req, Resp};
use crate::web::api_usage::{
    ErrorsQuery, ExportQuery, HeatQuery, LocalQuery, SeriesQuery, TopQuery,
};

fn base(method: &'static str, path: &'static str, summary: &'static str) -> DocRoute {
    DocRoute {
        method,
        path,
        tag: "usage",
        summary,
        description: "",
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::None,
        response: Resp::Untyped("BUG: usage.rs route builder did not override the response"),
        dialect: Dialect::Dashboard,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        example: None,
    }
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            description: "Cost and token series over a time range, bucketed and grouped by \
                class, alias, key or upstream. Series past `limit` (default 6, the chart's \
                colour slots) fold into a server-side \"other\" tail.",
            query: Some(|g| g.root_schema_for::<SeriesQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::UsageSeriesResponse>()),
            ..base("GET", "/api/usage/series", "Get a usage time series")
        },
        DocRoute {
            description: "The ranked rows for one dimension (alias, key, upstream, model) \
                over a time range, by cost then tokens.",
            query: Some(|g| g.root_schema_for::<TopQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::UsageTopResponse>()),
            ..base("GET", "/api/usage/top", "Get top usage by dimension")
        },
        DocRoute {
            description: "Cost by hour-of-day × day-of-week, for the usage heatmap.",
            query: Some(|g| g.root_schema_for::<HeatQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::UsageHeatResponse>()),
            ..base("GET", "/api/usage/heat", "Get a usage heatmap")
        },
        DocRoute {
            description: "Refusals and errors over a time range, grouped by error kind.",
            query: Some(|g| g.root_schema_for::<ErrorsQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::UsageErrorsResponse>()),
            ..base("GET", "/api/usage/errors", "Get usage errors by kind")
        },
        DocRoute {
            description: "The local/cloud spend split and the counterfactual: what the same \
                local traffic would have cost against the configured reference alias.",
            query: Some(|g| g.root_schema_for::<LocalQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::UsageLocalResponse>()),
            ..base("GET", "/api/usage/local", "Get the local/cloud usage split")
        },
        DocRoute {
            description: "Every gateway key with its lifetime and recent spend, scope and \
                last-used time.",
            response: Resp::Json(|g| g.root_schema_for::<dto::KeysResponse>()),
            ..base("GET", "/api/usage/keys", "List keys with their usage")
        },
        DocRoute {
            description: "Every price row, plus the models seen in traffic that have none — \
                the Prices tab's one read.",
            response: Resp::Json(|g| g.root_schema_for::<dto::PricesResponse>()),
            ..base("GET", "/api/usage/prices", "List prices")
        },
        DocRoute {
            description: "The same usage rows as GET /api/usage/series, as a CSV download — \
                one row per (bucket, key, alias, upstream, class, outcome).",
            query: Some(|g| g.root_schema_for::<ExportQuery>()),
            response: Resp::Binary(&["text/csv"]),
            ..base("GET", "/api/usage/export.csv", "Export usage as CSV")
        },
    ]
}
