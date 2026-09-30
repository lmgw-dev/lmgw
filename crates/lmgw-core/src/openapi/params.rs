//! Parameters: a route's path template translated to OpenAPI's spelling, the
//! path-parameter objects that go with it, a `Query<T>` struct turned into
//! query parameters, and the `x-lmgw-*` header parameters a route's method
//! and path pull out of [`super::headers::LMGW_HEADERS`] (api-docs design
//! §4.2, §4.9).

use schemars::SchemaGenerator;
use serde_json::{json, Value};

use lmgw_api_types::openapi_ext;

use super::headers::{Audience, Direction, HeaderSchema, Scope, LMGW_HEADERS};
use super::registry::SchemaFn;

/// `{*id}` becomes `{id}`: the wildcard sigil is lmgw's own routing syntax,
/// not OpenAPI's, which only ever has `{name}` (§4.2).
pub(crate) fn openapi_path(path: &str) -> String {
    path.replace("{*", "{")
}

/// One parameter object per `{name}` in `path` (the *original*, `{*name}`
/// spelling — that is what marks a wildcard). `path_ints` names the ones that
/// are `Path<i64>` in the handler; everything else is a plain string.
pub(crate) fn path_params(path: &str, path_ints: &[&str]) -> Vec<Value> {
    let mut out = Vec::new();
    for segment in path.split('/') {
        if !segment.starts_with('{') || !segment.ends_with('}') {
            continue;
        }
        let inner = &segment[1..segment.len() - 1];
        let (wildcard, name) = match inner.strip_prefix('*') {
            Some(rest) => (true, rest),
            None => (false, inner),
        };
        let schema_type = if path_ints.contains(&name) {
            "integer"
        } else {
            "string"
        };
        let mut obj = json!({
            "name": name,
            "in": "path",
            "required": true,
            "schema": { "type": schema_type },
        });
        if wildcard {
            obj[openapi_ext::WILDCARD] = json!(true);
        }
        out.push(obj);
    }
    out
}

/// A `Query<T>`'s top-level properties, one OpenAPI query parameter each.
///
/// `T`'s schema is generated with `g` (so its refs, if any, land in the same
/// `#/components/schemas`), then read back apart: OpenAPI has no "the query
/// string is this object", only a flat list of named parameters.
pub(crate) fn query_params(g: &mut SchemaGenerator, schema_fn: SchemaFn) -> Vec<Value> {
    let schema = super::schemas::embed(g, schema_fn);
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    props
        .iter()
        .map(|(name, prop_schema)| {
            json!({
                "name": name,
                "in": "query",
                "required": required.contains(&name.as_str()),
                "schema": prop_schema,
            })
        })
        .collect()
}

/// The `x-lmgw-*` request or response header parameters this `(method,
/// path)` carries, on the inference plane or not (`Scope::AllInference`
/// matches only when it is).
pub(crate) fn header_params(
    method: &str,
    path: &str,
    is_inference: bool,
    direction: Direction,
) -> Vec<Value> {
    LMGW_HEADERS
        .iter()
        .filter(|h| h.direction == direction)
        .filter(|h| h.audience != Audience::Internal)
        .filter(|h| match h.scope {
            Scope::None => false,
            Scope::AllInference => is_inference,
            Scope::Routes(rows) => rows.iter().any(|(m, p)| *m == method && *p == path),
        })
        .map(|h| {
            let schema = match h.schema {
                HeaderSchema::Enum(values) => json!({ "type": "string", "enum": values }),
                HeaderSchema::Integer => json!({ "type": "integer" }),
                HeaderSchema::Text => json!({ "type": "string" }),
            };
            // `x-lmgw-prefill` (§4.4) is not a property of the header itself
            // — `anthropic-version` prefills on Anthropic routes and nowhere
            // else, `Accept` prefills on `/mcp*` — so it is the caller's
            // (`build.rs`'s) call per route, not this table's.
            json!({
                "name": h.name,
                "in": "header",
                "required": false,
                "description": h.description,
                "schema": schema,
                (openapi_ext::AUDIENCE): h.audience.as_str(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_path_params_are_flagged_and_translated() {
        let params = path_params("/v1/models/{*id}", &[]);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0]["name"], "id");
        assert_eq!(params[0][openapi_ext::WILDCARD], true);
        assert_eq!(openapi_path("/v1/models/{*id}"), "/v1/models/{id}");
    }

    #[test]
    fn plain_and_integer_path_params() {
        let params = path_params("/api/agents/runs/{job_id}", &["job_id"]);
        assert_eq!(params[0]["name"], "job_id");
        assert_eq!(params[0]["schema"]["type"], "integer");
        assert!(params[0].get(openapi_ext::WILDCARD).is_none());

        let params = path_params("/api/agents/{id}", &[]);
        assert_eq!(params[0]["schema"]["type"], "string");
    }

    #[test]
    fn header_params_is_empty_off_the_inference_plane() {
        // Dashboard-plane routes carry no `x-lmgw-*` request header at all —
        // not even `x-lmgw-run` (`Scope::AllInference` reads `is_inference`).
        assert!(header_params("GET", "/api/status", false, Direction::Request).is_empty());
    }

    #[test]
    fn every_inference_route_carries_the_run_header() {
        // `x-lmgw-run`'s `Scope::AllInference` (headers.rs, WP2) matches any
        // inference route regardless of method or path, even one with no
        // more specific header of its own.
        let params = header_params("GET", "/v1/audio/voices", true, Direction::Request);
        assert_eq!(params.len(), 1, "{params:#?}");
        assert_eq!(params[0]["name"], "x-lmgw-run");
    }

    #[test]
    fn a_scoped_request_header_only_matches_its_own_routes() {
        // The reasoning trio (`Scope::Routes`) is on `/v1/chat/completions`
        // but not `/v1/completions`.
        let on = header_params("POST", "/v1/chat/completions", true, Direction::Request);
        assert!(on.iter().any(|p| p["name"] == "x-lmgw-reasoning"));
        let off = header_params("POST", "/v1/completions", true, Direction::Request);
        assert!(!off.iter().any(|p| p["name"] == "x-lmgw-reasoning"));
    }
}
