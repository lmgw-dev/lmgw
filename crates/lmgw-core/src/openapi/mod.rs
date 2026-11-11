//! The OpenAPI 3.1 description of lmgw's own HTTP API (api-docs design),
//! built in Rust from the sources of truth the rest of the crate already
//! carries — `server::CAPABILITY_TABLE`, the op dispatcher
//! (`web::op_names`), `schemars` schemas of the api-types DTOs — rather than
//! hand-maintained. Served twice (§4.11): the admin document at
//! `GET /api/openapi.json` is everything; the developer document at
//! `GET /v1/openapi.json` is every operation a non-owner credential reaches
//! (inference, the device Chat API, the agent run API) plus `GET /api/version`.
//!
//! This is the skeleton (WP1, §9): the registry types, the tag and exclusion
//! lists, the schema generator, the assembly and the two handlers. The
//! planes (`planes/*`), the op table (`ops/table.rs`) and the header table
//! (`headers.rs`) are empty until the work packages that own them fill them
//! in — everything here already works over an empty registry, so each lands
//! as a pure data change, not a structural one.

mod answer_defaults;
mod build;
mod descriptions;
mod endpoints;
mod exclusions;
mod headers;
mod ops;
mod params;
mod planes;
mod registry;
mod schemas;
#[cfg(test)]
mod sent_required_tests;
mod serve;
mod tags;
mod v1;

pub use build::{admin_doc, v1_doc};
pub use endpoints::lmgw_endpoints;
pub(crate) use headers::takes_background;
pub use headers::{
    headers_block, Audience, Direction, HeaderSchema, LmgwHeader, Scope, LMGW_HEADERS,
};
pub use ops::{Divergence, DIVERGENCES};
pub use serve::{admin_json, v1_json};

/// Op `name`'s `writes` and `reveals_secret` marks (§4.7), `None` for a
/// name the op table does not have: what the audit log reads to leave the
/// reading ops out (headless server design §5.8, D83).
pub(crate) fn op_marks(name: &str) -> Option<(bool, bool)> {
    static MARKS: std::sync::OnceLock<std::collections::HashMap<&'static str, (bool, bool)>> =
        std::sync::OnceLock::new();
    MARKS
        .get_or_init(|| {
            ops::table()
                .into_iter()
                .map(|o| (o.name, (o.writes, o.reveals_secret)))
                .collect()
        })
        .get(name)
        .copied()
}

/// The words an op, a `lmgw__*` tool or a documented route goes by (the op
/// table's and the route registry's `summary`): what the audit log's list
/// shows for an entry's action (headless server design §5.8).
pub(crate) struct Summaries {
    ops: std::collections::HashMap<&'static str, &'static str>,
    tools: std::collections::HashMap<&'static str, &'static str>,
    /// By method, then path.
    routes: std::collections::HashMap<
        &'static str,
        std::collections::HashMap<&'static str, &'static str>,
    >,
}

impl Summaries {
    pub(crate) fn get() -> &'static Self {
        static S: std::sync::OnceLock<Summaries> = std::sync::OnceLock::new();
        S.get_or_init(|| {
            let ops = ops::table();
            Summaries {
                ops: ops.iter().map(|o| (o.name, o.summary)).collect(),
                tools: ops
                    .iter()
                    .filter_map(|o| o.tool.map(|t| (t, o.summary)))
                    .collect(),
                routes: registry::all_routes().into_iter().fold(
                    std::collections::HashMap::new(),
                    |mut m: std::collections::HashMap<_, std::collections::HashMap<_, _>>, r| {
                        m.entry(r.method).or_default().insert(r.path, r.summary);
                        m
                    },
                ),
            }
        })
    }

    pub(crate) fn op(&self, name: &str) -> Option<&'static str> {
        self.ops.get(name).copied()
    }

    pub(crate) fn tool(&self, name: &str) -> Option<&'static str> {
        self.tools.get(name).copied()
    }

    pub(crate) fn of_route(&self, method: &str, path: &str) -> Option<&'static str> {
        self.routes.get(method)?.get(path).copied()
    }
}
