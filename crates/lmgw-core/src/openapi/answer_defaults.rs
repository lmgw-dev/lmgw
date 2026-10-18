//! An answer's schema carries no `default` keyword.
//!
//! The answer types are read tolerantly (`Default` + `#[serde(default)]`), and
//! schemars writes that into the schema as `default`s: `"default": ""` on an
//! enum field, a 700-character object on a nested struct. For a request a
//! `default` is a promise ("leave it out and this is what you get"); in an
//! answer the server always sends the field, so the keyword only misleads a
//! client generator and bloats the document. `schemas::apply_sent_required`
//! calls [`strip`] on every answer-side schema, and [`has`] decides whether a
//! component both sides reach has to split into its `<Name>Input` twin.

use serde_json::Value;

use super::descriptions::NAME_MAPS;

/// Remove every `default` keyword from the schema `value` (nested property,
/// item and composition schemas included). Keys of a name map (`properties`, `responses`, the SSE events, ...) are
/// names, so a field called `default` stays, and the values of
/// `enum`, `const`, `example` and `examples` are data, not schemas.
pub(crate) fn strip(value: &mut Value) {
    walk(value, false);
}

/// Whether the schema `value` carries a `default` keyword anywhere [`strip`]
/// would remove one.
pub(crate) fn has(value: &Value) -> bool {
    let mut copy = value.clone();
    strip(&mut copy);
    &copy != value
}

fn walk(value: &mut Value, names: bool) {
    match value {
        Value::Object(map) => {
            if !names {
                map.remove("default");
            }
            for (key, child) in map.iter_mut() {
                if !names && matches!(key.as_str(), "enum" | "const" | "example" | "examples") {
                    continue;
                }
                // The children of these maps are schemas under their names.
                walk(child, !names && NAME_MAPS.contains(&key.as_str()));
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| walk(v, false)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn strips_keywords_and_keeps_names_and_data() {
        let mut schema = json!({
            "type": "object",
            "default": {},
            "properties": {
                "default": { "type": "string", "default": "x" },
                "kind": { "enum": ["a"], "default": "a", "const": { "default": 1 } },
                "list": { "type": "array", "items": { "type": "integer", "default": 0 } },
            },
            "oneOf": [{ "type": "null", "default": null }],
            "example": { "default": 2 },
        });
        assert!(has(&schema));
        strip(&mut schema);
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "properties": {
                    "default": { "type": "string" },
                    "kind": { "enum": ["a"], "const": { "default": 1 } },
                    "list": { "type": "array", "items": { "type": "integer" } },
                },
                "oneOf": [{ "type": "null" }],
                "example": { "default": 2 },
            })
        );
        assert!(!has(&schema));
    }
}
