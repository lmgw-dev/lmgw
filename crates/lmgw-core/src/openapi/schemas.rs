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

/// The schemas that describe what the gateway *sends* (a response body, an
/// event frame), generated a second time in the serialize contract; see
/// [`Sent::embed`] and [`apply_sent_required`].
pub(crate) struct Sent {
    gs: SchemaGenerator,
}

impl Sent {
    pub(crate) fn new() -> Self {
        Self {
            gs: sent_generator(),
        }
    }

    /// [`embed`] for a sent schema: the same value, its `required` lists
    /// corrected in place where the schema is inline; the components it
    /// reaches are corrected at the end by [`apply_sent_required`].
    pub(crate) fn embed(&mut self, g: &mut SchemaGenerator, f: SchemaFn) -> Value {
        let mut value = embed(g, f);
        sync_required(&mut value, &embed(&mut self.gs, f));
        value
    }

    /// Register `f`'s components as sent without embedding it anywhere.
    pub(crate) fn touch(&mut self, f: SchemaFn) {
        f(&mut self.gs);
    }
}

/// The *serialize* contract twin of [`generator`]: a field is optional only
/// where `skip_serializing_if` can leave it out, whatever the reader's
/// `#[serde(default)]` says.
fn sent_generator() -> SchemaGenerator {
    SchemaSettings::draft2020_12()
        .for_serialize()
        .with(|s| {
            s.meta_schema = None;
            s.definitions_path = "/components/schemas".into();
        })
        .into_generator()
}

/// Make the `required` lists of every component the gateway sends say what the
/// server always sends.
///
/// [`generator`] reads the *deserialize* contract, which is right for a
/// request (a `#[serde(default)]` field really is optional there) but wrong
/// for an answer: the readers of the answer types are tolerant (a container
/// level `#[serde(default)]` keeps an older UI or client working across
/// version skew), and that left the generated schemas without any `required`,
/// so a client generator made `id`, `role`, `content` optional although the
/// server always sends them. The same types are generated again in the
/// serialize contract (a field is optional only where `skip_serializing_if`
/// can leave it out) and the `required` lists of every component found there
/// replace the reader's, wire and readers untouched. Only the lists change:
/// a property's type keeps whatever null it states. "Optional" does not mean
/// "never null" (a flattened `Option` is optional in the serialize contract
/// while its fields are sent as `null`, and a hand-written schema states its
/// own nullability), so a field that really is never `null` says so itself
/// with `openapi_ext::non_null`.
///
/// A component a request also reaches (`reads`) has one name but two truths.
/// Where its two lists differ, the request side keeps the reader's list under
/// the name `<Name>Input` (every request `$ref` is rewritten, as is every
/// shared component that refers to one) and the plain name becomes the
/// answer's. Returns the names that were split, for the test that lists them.
pub(crate) fn apply_sent_required(
    g: &mut SchemaGenerator,
    sent: Sent,
    reads: &BTreeSet<String>,
    paths: &mut serde_json::Map<String, Value>,
) -> BTreeSet<String> {
    let sent_defs = sent.gs.definitions();
    // The components both sides reach whose two views differ.
    let mut split: BTreeSet<String> = BTreeSet::new();
    for (name, ser) in sent_defs {
        if !reads.contains(name) {
            continue;
        }
        if let Some(de) = g.definitions().get(name) {
            let mut synced = de.clone();
            sync_required(&mut synced, ser);
            if &synced != de {
                split.insert(name.clone());
            }
        }
    }
    // ... and those that refer to one: the request side has to refer to the
    // `Input` twin, so they split as well.
    loop {
        let mut added = Vec::new();
        for name in reads.iter().filter(|n| sent_defs.contains_key(*n)) {
            if split.contains(name) {
                continue;
            }
            let mut refs = Vec::new();
            if let Some(d) = g.definitions().get(name) {
                collect_refs(d, &mut refs);
            }
            if refs.iter().any(|r| split.contains(r)) {
                added.push(name.clone());
            }
        }
        if added.is_empty() {
            break;
        }
        split.extend(added);
    }
    // The request side's twins, and every request-side `$ref` pointed at them.
    for name in &split {
        let Some(mut twin) = g.definitions().get(name).cloned() else {
            continue;
        };
        rewrite_refs(&mut twin, &split);
        g.definitions_mut().insert(format!("{name}Input"), twin);
    }
    for name in reads.iter().filter(|n| !split.contains(*n)) {
        if let Some(d) = g.definitions_mut().get_mut(name) {
            rewrite_refs(d, &split);
        }
    }
    for methods in paths.values_mut().filter_map(Value::as_object_mut) {
        for op in methods.values_mut().filter_map(Value::as_object_mut) {
            for key in ["requestBody", "parameters"] {
                if let Some(v) = op.get_mut(key) {
                    rewrite_refs(v, &split);
                }
            }
        }
    }
    // The answer's lists.
    for (name, ser) in sent_defs {
        if let Some(de) = g.definitions_mut().get_mut(name) {
            sync_required(de, ser);
        }
    }
    split
}

/// Point every `$ref` to one of `names` at its `Input` twin.
fn rewrite_refs(value: &mut Value, names: &BTreeSet<String>) {
    const PREFIX: &str = "#/components/schemas/";
    match value {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get_mut("$ref") {
                if let Some(name) = r.strip_prefix(PREFIX) {
                    if names.contains(name) {
                        *r = format!("{PREFIX}{name}Input");
                    }
                }
            }
            for v in map.values_mut() {
                rewrite_refs(v, names);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| rewrite_refs(v, names)),
        _ => {}
    }
}

/// Copy `ser`'s `required` lists over `de`'s, walking the two schemas side by
/// side (they differ in nothing else that matters here), each list cut to the
/// properties `de` has.
fn sync_required(de: &mut Value, ser: &Value) {
    let (Value::Object(de), Value::Object(ser)) = (de, ser) else {
        return;
    };
    if let (Some(Value::Object(de_props)), Some(Value::Object(_))) =
        (de.get("properties"), ser.get("properties"))
    {
        let required: Vec<Value> = ser
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|n| n.as_str().is_some_and(|n| de_props.contains_key(n)))
            .cloned()
            .collect();
        if required.is_empty() {
            de.remove("required");
        } else {
            de.insert("required".to_string(), Value::Array(required));
        }
    }
    if let (Some(Value::Object(de_props)), Some(Value::Object(ser_props))) =
        (de.get_mut("properties"), ser.get("properties"))
    {
        for (name, de_prop) in de_props.iter_mut() {
            if let Some(ser_prop) = ser_props.get(name) {
                sync_required(de_prop, ser_prop);
            }
        }
    }
    for key in ["oneOf", "anyOf", "allOf", "prefixItems"] {
        if let (Some(Value::Array(des)), Some(Value::Array(sers))) = (de.get_mut(key), ser.get(key))
        {
            for (d, s) in des.iter_mut().zip(sers) {
                sync_required(d, s);
            }
        }
    }
    for key in ["items", "additionalProperties"] {
        if let (Some(d), Some(s)) = (de.get_mut(key), ser.get(key)) {
            sync_required(d, s);
        }
    }
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
pub(crate) fn collect_refs(value: &Value, out: &mut Vec<String>) {
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
