//! A collapsing-free schema tree (api-docs design §6.2 "Documentation"
//! panel, §6.3): type, required, enum, default, description, with `$ref`s
//! resolved against the document's own components — a ref already on the
//! path renders "↻ Name (recursive)" instead of recursing forever. A plain
//! recursive function rather than a `#[component]`: Leptos components cannot
//! call themselves directly, and every node here is rendered once from data
//! already in hand, with nothing of its own to react to.

use leptos::prelude::*;
use serde_json::Value;

use lmgw_api_types::openapi_ext as ext;

use super::doc::resolve_ref;

/// Render `schema` (possibly a `$ref`) as a tree. `required` says whether the
/// *parent* required this property — the root call passes `false`, there
/// being no parent to require it.
pub fn schema_view(schema: &Value, components: &Value, path: &[String], required: bool) -> AnyView {
    if let Some((name, resolved)) = resolve_ref(schema, components) {
        if path.iter().any(|p| p == &name) {
            return view! { <span class="sv-recursive">{format!("↻ {name} (recursive)")}</span> }
                .into_any();
        }
        let mut next = path.to_vec();
        next.push(name.clone());
        return view! {
            <div class="sv-node">
                <div class="sv-ref-head">
                    <code class="sv-ref" title=format!("#/components/schemas/{name}")>{name.clone()}</code>
                    {required.then(|| view! { <span class="sv-req">"required"</span> })}
                </div>
                {schema_view(resolved, components, &next, false)}
            </div>
        }
        .into_any();
    }
    render_inline(schema, components, path, required)
}

fn schema_kind(schema: &Value) -> String {
    if let Some(t) = schema.get("type").and_then(Value::as_str) {
        return t.to_string();
    }
    if schema.get("oneOf").is_some() {
        "oneOf".to_string()
    } else if schema.get("anyOf").is_some() {
        "anyOf".to_string()
    } else if schema.get("allOf").is_some() {
        "allOf".to_string()
    } else if schema.get("properties").is_some() {
        "object".to_string()
    } else {
        "any".to_string()
    }
}

fn render_inline(schema: &Value, components: &Value, path: &[String], required: bool) -> AnyView {
    let ty = schema_type_label(schema);
    let description = schema
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string);
    let default = schema.get("default").cloned();
    let enum_vals: Option<String> = schema.get("enum").and_then(Value::as_array).map(|a| {
        a.iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            })
            .collect::<Vec<_>>()
            .join(", ")
    });
    let untyped = schema
        .get(ext::UNTYPED)
        .and_then(Value::as_str)
        .map(str::to_string);
    let secret = schema
        .get(ext::SECRET)
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let body = alt_body(schema, components, path)
        .or_else(|| object_body(schema, components, path))
        .or_else(|| array_body(schema, components, path));

    view! {
        <div class="sv-node">
            <div class="sv-head">
                <span class="sv-type">{ty}</span>
                {required.then(|| view! { <span class="sv-req">"required"</span> })}
                {secret.then(|| view! { <span class="chip warn sm" title="never sent in a curl">"secret"</span> })}
                {enum_vals.map(|vs| view! { <span class="sv-enum">"enum: " {vs}</span> })}
                {default.as_ref().map(|d| view! { <span class="sv-default">"default: " {d.to_string()}</span> })}
            </div>
            {description.map(|d| view! { <p class="sv-desc dim">{d}</p> })}
            {untyped.map(|w| view! { <p class="sv-desc dim">"untyped: " {w}</p> })}
            {body}
        </div>
    }
    .into_any()
}

fn schema_type_label(schema: &Value) -> String {
    schema_kind(schema)
}

/// `oneOf`/`anyOf`/`allOf`: each branch, one under the other.
fn alt_body(schema: &Value, components: &Value, path: &[String]) -> Option<AnyView> {
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(branches) = schema.get(key).and_then(Value::as_array) {
            let rows: Vec<AnyView> = branches
                .iter()
                .map(|b| schema_view(b, components, path, false))
                .collect();
            return Some(
                view! {
                    <div class="sv-alt">
                        <span class="sv-alt-label">{key}</span>
                        <div class="sv-props">{rows}</div>
                    </div>
                }
                .into_any(),
            );
        }
    }
    None
}

fn object_body(schema: &Value, components: &Value, path: &[String]) -> Option<AnyView> {
    let props = schema.get("properties")?.as_object()?;
    if props.is_empty() {
        return None;
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let rows: Vec<AnyView> = props
        .iter()
        .map(|(name, prop)| {
            let is_req = required.contains(&name.as_str());
            view! {
                <div class="sv-prop">
                    <code class="sv-prop-name">{name.clone()}</code>
                    {schema_view(prop, components, path, is_req)}
                </div>
            }
            .into_any()
        })
        .collect();
    Some(view! { <div class="sv-props">{rows}</div> }.into_any())
}

fn array_body(schema: &Value, components: &Value, path: &[String]) -> Option<AnyView> {
    let items = schema.get("items")?;
    Some(
        view! {
            <div class="sv-items">
                <span class="sv-items-label">"items:"</span>
                {schema_view(items, components, path, false)}
            </div>
        }
        .into_any(),
    )
}
