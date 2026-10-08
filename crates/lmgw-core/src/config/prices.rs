//! Price rows and the scope-key helper usage-cost lookups share.

use serde::{Deserialize, Serialize};

pub use lmgw_api_types::PriceUnit;

/// One row of `prices`: what a scope costs in one unit (billable-units design
/// §2.2).
///
/// A `per_mtok` row carries the four token rates, per 1M tokens, and no
/// `price`; a row in any other unit carries `price`, per the unit's scale, and
/// no token rate. The table's CHECK holds that split.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceRow {
    pub id: i64,
    pub scope_kind: PriceScope,
    /// The alias name, or `"<upstream_id>:<upstream_model_id>"`.
    pub scope_key: String,
    pub unit: PriceUnit,
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    pub price_cache_read: Option<f64>,
    pub price_cache_write: Option<f64>,
    pub source: crate::pricing::PriceSource,
    pub note: Option<String>,
    pub updated_at: String,
    /// The rate of a unit other than tokens ([`PriceUnit::scale`]); `None` on
    /// a `per_mtok` row.
    pub price: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceScope {
    Alias,
    UpstreamModel,
}

impl PriceScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alias => "alias",
            Self::UpstreamModel => "upstream_model",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "upstream_model" => Self::UpstreamModel,
            _ => Self::Alias,
        }
    }
}

/// The key under which a passthrough (no alias row) request is priced.
pub fn upstream_scope_key(upstream_id: i64, model: &str) -> String {
    format!("{upstream_id}:{model}")
}
