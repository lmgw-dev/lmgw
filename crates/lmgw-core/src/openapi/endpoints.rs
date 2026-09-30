//! `lmgw_endpoints()` (api-docs design §4.11): `/v1/models`' `lmgw.endpoints`
//! block, generated from the inference plane's own route registry instead of
//! hand-maintained in `server.rs` — so it can never again advertise a route
//! that does not exist (§0 finding 6: it used to name
//! `/v1/messages/count_tokens` and `/tokenize` before either was registered).

use serde_json::{json, Value};

use lmgw_api_types::openapi_ext::endpoint_group;

use super::params;
use super::planes;

/// The `{openai, anthropic, other}` groups: for each inference-plane
/// `DocRoute`, its OpenAPI path (`{*id}` translated, same as every other
/// path in the document) goes into every group its own `endpoints` names, in
/// registry order, each path once per group.
///
/// Builds no schemas — this only reads `DocRoute::path`/`::endpoints`, so it
/// is cheap enough to call on every `/v1/models` response rather than caching
/// it alongside the (expensive, `OnceLock`-cached) OpenAPI document.
///
/// `pub`, not `pub(crate)`: `openapi/mod.rs` re-exports it (`pub use
/// endpoints::lmgw_endpoints`) alongside `admin_doc`/`v1_doc`/`headers_block`,
/// which are the same kind of "read the registry, not the document" helper.
pub fn lmgw_endpoints() -> Value {
    let mut openai: Vec<String> = Vec::new();
    let mut anthropic: Vec<String> = Vec::new();
    let mut other: Vec<String> = Vec::new();

    for route in planes::inference::routes() {
        let path = params::openapi_path(route.path);
        for group in route.endpoints {
            let bucket = match *group {
                endpoint_group::OPENAI => &mut openai,
                endpoint_group::ANTHROPIC => &mut anthropic,
                endpoint_group::OTHER => &mut other,
                other => panic!(
                    "openapi: {} {} names an unknown lmgw.endpoints group '{other}'",
                    route.method, route.path
                ),
            };
            if !bucket.contains(&path) {
                bucket.push(path.clone());
            }
        }
    }

    json!({ "openai": openai, "anthropic": anthropic, "other": other })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_that_declares_a_group_is_listed_once() {
        let block = lmgw_endpoints();
        let openai = block["openai"].as_array().unwrap();
        assert!(
            openai.iter().any(|v| v == "/v1/chat/completions"),
            "{openai:?}"
        );
        assert!(openai.iter().any(|v| v == "/v1/models/{id}"), "{openai:?}");
        let anthropic = block["anthropic"].as_array().unwrap();
        assert!(
            anthropic.iter().any(|v| v == "/v1/messages"),
            "{anthropic:?}"
        );
        assert!(
            anthropic.iter().any(|v| v == "/v1/messages/count_tokens"),
            "{anthropic:?}"
        );
        let other = block["other"].as_array().unwrap();
        for path in [
            "/v1/responses/{id}",
            "/v1/responses/{id}/input_items",
            "/v1/openapi.json",
            "/tokenize",
        ] {
            assert!(other.iter().any(|v| v == path), "{path} missing: {other:?}");
        }
        // `/mcp*` routes carry no `endpoints` group and must not leak in.
        for bucket in [openai, anthropic, other] {
            assert!(
                !bucket
                    .iter()
                    .any(|v| v.as_str().is_some_and(|s| s.starts_with("/mcp"))),
                "{bucket:?}"
            );
        }
    }

    #[test]
    fn a_path_in_two_groups_is_not_duplicated_within_one() {
        let block = lmgw_endpoints();
        let openai = block["openai"].as_array().unwrap();
        let count = openai.iter().filter(|v| *v == "/v1/models").count();
        assert_eq!(count, 1, "{openai:?}");
    }
}
