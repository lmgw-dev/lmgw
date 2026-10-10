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

mod build;
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
pub use headers::{
    headers_block, Audience, Direction, HeaderSchema, LmgwHeader, Scope, LMGW_HEADERS,
};
pub use ops::{Divergence, DIVERGENCES};
pub use serve::{admin_json, v1_json};
