//! Schema generation: the one [`generator`] config every plane, op and
//! hand-written `/v1` schema shares (api-docs design §4.5), named-schema
//! registration, the small untyped-response helper, error-envelope refs, and
//! [`prune_components`].

use std::collections::BTreeSet;

use schemars::generate::{SchemaGenerator, SchemaSettings};
use schemars::Schema;
use serde_json::{json, Value};

use lmgw_api_types::openapi_ext;

use super::registry::{Dialect, SchemaFn};

/// Draft 2020-12, refs into `#/components/schemas` (so a ref reads
/// `#/components/schemas/Name`, an OpenAPI 3.1 document's own convention),
/// the *deserialize* contract (this is the dashboard's reading contract:
/// `#[serde(default)]` fields are optional, and unknown keys are allowed
/// unless `deny_unknown_fields`), and no top-level `$schema` keyword — the
/// document's own `openapi` version key is what says which dialect this is.
pub(crate) fn generator() -> SchemaGenerator {
    SchemaSettings::draft2020_12()
        .for_deserialize()
        .with(|s| {
            s.meta_schema = None;
            s.definitions_path = "/components/schemas".into();
        })
        .into_generator()
}

/// A [`SchemaFn`]'s schema, ready to sit inside an operation: `f(g)` with the
/// definitions copy `root_schema_for` puts inside every root it returns taken
/// back out.
///
/// schemars' `root_schema_for` answers a *standalone* schema, so it copies
/// every definition the shared generator has built so far into the returned
/// object at its definitions path (`components/schemas`, the
/// [`generator`] setting). Inside this document that copy is pure
/// duplication: each of those definitions is still in `g` and lands exactly
/// once, in the root `components.schemas` `build.rs` assembles, which is where
/// a `#/components/schemas/X` ref resolves from anywhere in the document.
/// Left in, the copies grew with every route built — the admin document
/// carried 125 of them, about 195 KB each, and weighed 16 MB (review R2 #1).
/// Every call site of a `SchemaFn` goes through here.
pub(crate) fn embed(g: &mut SchemaGenerator, f: SchemaFn) -> Value {
    let mut value = f(g).to_value();
    if let Value::Object(map) = &mut value {
        map.remove("components");
    }
    value
}

/// Register a hand-written schema under `name` in `g`'s
/// `#/components/schemas`, and return a `$ref` to it.
///
/// Panics on a name already registered with *different* content: every
/// component name is unique (§4.5), and letting a second, quietly different
/// definition through would be the one place two routes describing the same
/// shape drifted apart without either being wrong on its own.
pub(crate) fn named(g: &mut SchemaGenerator, name: &str, schema: Schema) -> Schema {
    let value = schema.to_value();
    let defs = g.definitions_mut();
    match defs.get(name) {
        Some(existing) if *existing != value => panic!(
            "openapi: schema '{name}' registered twice with different content — \
             every component name must be unique (api-docs design §4.5)"
        ),
        _ => {
            defs.insert(name.to_string(), value);
        }
    }
    named_ref(name)
}

/// A `$ref` to a schema already registered by name — `named`'s other half,
/// for a second route that reaches the same component `named` already put
/// there.
pub(crate) fn named_ref(name: &str) -> Schema {
    schemars::json_schema!({ "$ref": format!("#/components/schemas/{name}") })
}

/// `Resp::Untyped(why)`'s schema: an opaque object, with the reason it has no
/// shape (§4.5).
pub(crate) fn untyped(why: &str) -> Value {
    json!({ "type": "object", (openapi_ext::UNTYPED): why })
}

/// A `$ref` to the dialect's error envelope (§4.3 `default` response, §4.8),
/// registering it in `g` on first use via [`super::v1::errors::register`].
///
/// **WP1 bug, fixed in WP6.** This used to build the `$ref` string by hand
/// with no `SchemaGenerator` access at all, so nothing ever put the schema
/// itself into `components/schemas` — every operation's `default` response
/// pointed at a component that would never exist, which
/// `doc_is_structurally_sound`'s "every `$ref` resolves" catches the moment a
/// real route is built (WP1 had none). Threading `g` through, and having this
/// call into `v1::errors` the same way [`named`] does for any other
/// hand-written schema, is what makes "the first route of that dialect built"
/// literally true rather than aspirational.
pub(crate) fn error_ref(g: &mut SchemaGenerator, dialect: Dialect) -> Value {
    super::v1::errors::register(g, dialect).to_value()
}

/// Drop every `#/components/schemas/*` entry nothing in `paths` reaches,
/// following `$ref` transitively through the schemas that *are* kept (§4.5:
/// the v1 document is the admin document filtered to inference operations,
/// then pruned).
pub(crate) fn prune_components(doc: &mut Value) {
    let reachable = reachable_schema_names(doc);
    if let Some(schemas) = doc
        .pointer_mut("/components/schemas")
        .and_then(Value::as_object_mut)
    {
        schemas.retain(|name, _| reachable.contains(name.as_str()));
    }
}

fn reachable_schema_names(doc: &Value) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut frontier: Vec<String> = Vec::new();
    if let Some(paths) = doc.get("paths") {
        collect_refs(paths, &mut frontier);
    }
    while let Some(name) = frontier.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(schema) = doc.pointer(&format!("/components/schemas/{name}")) {
            collect_refs(schema, &mut frontier);
        }
    }
    seen
}

/// Every `#/components/schemas/<name>` a `$ref` string in `value` names,
/// recursively — `value` is walked whole, so a ref nested inside `oneOf`,
/// `properties`, an array item, anywhere, is found.
fn collect_refs(value: &Value, out: &mut Vec<String>) {
    const PREFIX: &str = "#/components/schemas/";
    match value {
        Value::String(s) => {
            if let Some(name) = s.strip_prefix(PREFIX) {
                out.push(name.to_string());
            }
        }
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get("$ref") {
                if let Some(name) = s.strip_prefix(PREFIX) {
                    out.push(name.to_string());
                }
            }
            for v in map.values() {
                collect_refs(v, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_refs(v, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_registers_once_and_refs_the_same_name() {
        let mut g = generator();
        let a = named(&mut g, "Widget", schemars::json_schema!({"type": "string"}));
        let b = named(&mut g, "Widget", schemars::json_schema!({"type": "string"}));
        assert_eq!(a.as_value(), b.as_value());
        assert_eq!(g.definitions().len(), 1);
    }

    #[test]
    #[should_panic(expected = "registered twice with different content")]
    fn named_panics_on_a_colliding_redefinition() {
        let mut g = generator();
        named(&mut g, "Widget", schemars::json_schema!({"type": "string"}));
        named(&mut g, "Widget", schemars::json_schema!({"type": "number"}));
    }

    #[test]
    fn untyped_carries_its_reason() {
        let v = untyped("no DTO exists");
        assert_eq!(v["x-lmgw-untyped"], "no DTO exists");
        assert_eq!(v["type"], "object");
    }

    #[test]
    fn prune_drops_schemas_nothing_reaches() {
        let mut doc = json!({
            "paths": {
                "/x": {"get": {"responses": {"200": {"content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/Kept"}
                }}}}}}
            },
            "components": {"schemas": {
                "Kept": {"type": "object", "properties": {
                    "nested": {"$ref": "#/components/schemas/AlsoKept"}
                }},
                "AlsoKept": {"type": "string"},
                "Orphan": {"type": "string"},
            }}
        });
        prune_components(&mut doc);
        let names: BTreeSet<String> = doc["components"]["schemas"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            names,
            BTreeSet::from(["Kept".to_string(), "AlsoKept".to_string()])
        );
    }
}
