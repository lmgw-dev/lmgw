//! Request-body examples (api-docs design §4.10, §6.6): extract a schema's
//! own `example` when it has one, else generate one from its required
//! properties — and the `model` field get/set the tester's `ModelPicker`
//! integration needs. Pure text/JSON manipulation; no signals.

use serde_json::{json, Map, Value};

use super::doc::resolve_ref;

/// The example for a request-body `schema` (possibly a `$ref`): its own
/// `"example"` if it carries one — beside the `$ref` (where `build.rs` puts
/// a route's or op's example, §4.10) or on the component it names — else
/// one generated from `required` properties.
pub fn body_example(schema: &Value, components: &Value) -> Value {
    if let Some(ex) = schema.get("example") {
        return ex.clone();
    }
    let resolved = resolve_ref(schema, components)
        .map(|(_, r)| r.clone())
        .unwrap_or_else(|| schema.clone());
    if let Some(ex) = resolved.get("example") {
        return ex.clone();
    }
    generate(&resolved, components)
}

/// One required property at a time: the first `enum` value, else `"<name>"`,
/// `1`, `false`, `{}` or `[]` by type (§4.10). `action` goes first when it is
/// among the required properties.
fn generate(schema: &Value, components: &Value) -> Value {
    let resolved = resolve_ref(schema, components)
        .map(|(_, r)| r.clone())
        .unwrap_or_else(|| schema.clone());
    let Some(props) = resolved.get("properties").and_then(Value::as_object) else {
        return json!({});
    };
    let required: Vec<&str> = resolved
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut keys = required;
    // Stable sort: `action` first, everything else keeps its schema order.
    keys.sort_by_key(|k| if *k == "action" { 0 } else { 1 });

    let mut obj = Map::new();
    for key in keys {
        if let Some(prop) = props.get(key) {
            obj.insert(key.to_string(), example_for_property(key, prop, components));
        }
    }
    Value::Object(obj)
}

fn example_for_property(name: &str, schema: &Value, components: &Value) -> Value {
    let resolved = resolve_ref(schema, components).map_or(schema, |(_, r)| r);
    if let Some(first) = resolved
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        return first.clone();
    }
    match resolved.get("type").and_then(Value::as_str) {
        Some("integer") | Some("number") => json!(1),
        Some("boolean") => json!(false),
        Some("object") => json!({}),
        Some("array") => json!([]),
        _ => json!(format!("<{name}>")),
    }
}

/// The top-level `model` field of a JSON body text, if it parses and has one
/// (§6.6: the `ModelPicker` "follows the text when it parses").
pub fn get_model(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()?
        .get("model")?
        .as_str()
        .map(String::from)
}

/// Write the top-level `model` field, re-pretty-printed. `None` when `text`
/// does not parse as a JSON object — the caller leaves the text untouched and
/// disables the picker with a hint (§6.6).
pub fn set_model(text: &str, model: &str) -> Option<String> {
    let mut v: Value = serde_json::from_str(text).ok()?;
    v.as_object_mut()?.insert("model".to_string(), json!(model));
    serde_json::to_string_pretty(&v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_example_wins_over_generation() {
        let schema = json!({
            "type": "object",
            "example": {"model": "picked-one"},
            "required": ["model"],
            "properties": {"model": {"type": "string"}},
        });
        assert_eq!(
            body_example(&schema, &json!({})),
            json!({"model": "picked-one"})
        );
    }

    #[test]
    fn generation_covers_every_type_and_puts_action_first() {
        let schema = json!({
            "type": "object",
            "required": ["id", "action", "enabled", "count", "tags", "extra"],
            "properties": {
                "id": {"type": "string"},
                "action": {"type": "string", "enum": ["create", "update"]},
                "enabled": {"type": "boolean"},
                "count": {"type": "integer"},
                "tags": {"type": "array"},
                "extra": {"type": "object"},
            },
        });
        let ex = body_example(&schema, &json!({}));
        let obj = ex.as_object().unwrap();
        assert_eq!(obj.keys().next().unwrap(), "action");
        assert_eq!(ex["action"], "create");
        assert_eq!(ex["id"], "<id>");
        assert_eq!(ex["enabled"], false);
        assert_eq!(ex["count"], 1);
        assert_eq!(ex["tags"], json!([]));
        assert_eq!(ex["extra"], json!({}));
    }

    #[test]
    fn an_example_beside_a_ref_wins_over_the_component() {
        // `build.rs` sets `schema["example"]` on the `{"$ref": …}` value
        // `schemas::named` returns.
        let components = json!({"schemas": {"Chat": {
            "type": "object", "required": ["model"], "properties": {"model": {"type": "string"}},
        }}});
        let schema = json!({
            "$ref": "#/components/schemas/Chat",
            "example": {"model": "m", "messages": []},
        });
        assert_eq!(
            body_example(&schema, &components),
            json!({"model": "m", "messages": []})
        );
    }

    #[test]
    fn generation_follows_a_ref() {
        let components = json!({"schemas": {"Patch": {
            "type": "object", "required": ["name"], "properties": {"name": {"type": "string"}},
        }}});
        let schema = json!({"$ref": "#/components/schemas/Patch"});
        assert_eq!(
            body_example(&schema, &components),
            json!({"name": "<name>"})
        );
    }

    #[test]
    fn model_get_and_set_round_trip_through_pretty_text() {
        assert_eq!(get_model(r#"{"model":"a","x":1}"#), Some("a".to_string()));
        assert_eq!(get_model("not json"), None);
        assert_eq!(get_model(r#"{"x":1}"#), None);

        let updated = set_model(r#"{"model":"a","x":1}"#, "b").unwrap();
        assert_eq!(get_model(&updated), Some("b".to_string()));
        assert_eq!(set_model("not json", "b"), None);
    }
}
