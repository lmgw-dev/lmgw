//! The "Documentation" column (api-docs design §6.2, §6.3 `section.api-doc`):
//! description, parameters, request body, responses and their headers, and
//! SSE events — everything read straight off the operation's own OpenAPI
//! object, via `schema_view` for every schema tree.

use leptos::prelude::*;
use serde_json::Value;

use crate::widgets::{ClampText, Section};

use super::doc::Operation;
use super::schema_view::schema_view;

#[component]
pub fn OperationDoc(op: Operation, components: Value) -> impl IntoView {
    let params = op.parameters().to_vec();
    let body = op
        .request_body_schema()
        .map(|(mime, schema)| (mime.to_string(), schema.clone()));
    let body_required = op.request_body_required();
    let responses: Vec<(String, Value)> = op
        .responses()
        .map(|(status, resp)| (status.to_string(), resp.clone()))
        .collect();
    let sse_events: Vec<(String, Value)> = op
        .sse_events()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let description = op.description.clone();
    let components = StoredValue::new(components);

    // `Section`'s children run inside a `ChildrenFn` (`Fn() -> AnyView`,
    // re-invoked whenever the section's own show/hide state recomputes), so
    // nothing owned above may be *moved* out of this closure — every `move
    // ||` built below closes over its own clone, taken fresh each time this
    // whole block runs, never the outer capture itself.
    view! {
        <Section title="Documentation" persist="api_ref.doc">
            <div class="sv-block">
                {(!description.is_empty()).then({
                    let description = description.clone();
                    || view! { <ClampText text=description lines=4/> }
                })}

                {(!params.is_empty()).then({
                    let params = params.clone();
                    move || {
                        view! {
                            <h4 class="sv-h">"Parameters"</h4>
                            <table class="data kv-table sv-params">
                                <tbody>
                                    <For each=move || params.clone() key=|p| p.to_string() let:p>
                                        {render_param_row(p)}
                                    </For>
                                </tbody>
                            </table>
                        }
                    }
                })}

                {body.clone().map(|(mime, schema)| {
                    view! {
                        <h4 class="sv-h">
                            "Request body"
                            <span class="dim">
                                " · " {mime} {if body_required { " · required" } else { " · optional" }}
                            </span>
                        </h4>
                        {components.with_value(|c| schema_view(&schema, c, &[], body_required))}
                    }
                })}

                <h4 class="sv-h">"Responses"</h4>
                <div class="sv-responses">
                    {
                        let responses = responses.clone();
                        view! {
                            <For each=move || responses.clone() key=|(s, _)| s.clone() let:row>
                                {render_response(row, components)}
                            </For>
                        }
                    }
                </div>

                {(!sse_events.is_empty()).then({
                    let sse_events = sse_events.clone();
                    move || {
                        view! {
                            <h4 class="sv-h">"SSE events"</h4>
                            <div class="sv-responses">
                                <For each=move || sse_events.clone() key=|(n, _)| n.clone() let:ev>
                                    {
                                        let (name, schema) = ev;
                                        view! {
                                            <div class="sv-resp">
                                                <div class="sv-resp-head">
                                                    <code>{name}</code>
                                                </div>
                                                {components.with_value(|c| schema_view(&schema, c, &[], false))}
                                            </div>
                                        }
                                    }
                                </For>
                            </div>
                        }
                    }
                })}
            </div>
        </Section>
    }
}

fn render_param_row(p: Value) -> impl IntoView {
    let name = p
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let loc = p
        .get("in")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let required = p.get("required").and_then(Value::as_bool).unwrap_or(false);
    let ty = p
        .get("schema")
        .and_then(|s| s.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("string")
        .to_string();
    let description = p
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let wildcard = p
        .get(lmgw_api_types::openapi_ext::WILDCARD)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    view! {
        <tr>
            <td class="mono-sm">
                {name}
                {required.then(|| view! { <span class="sv-req">" required"</span> })}
                {wildcard.then(|| view! { <span class="dim">" (wildcard)"</span> })}
            </td>
            <td class="dim">{loc}</td>
            <td class="dim">{ty}</td>
            <td class="dim wrap">{description}</td>
        </tr>
    }
}

fn render_response(row: (String, Value), components: StoredValue<Value>) -> impl IntoView {
    let (status, resp) = row;
    let is_default = status == "default";
    let description = resp
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let schema = resp.pointer("/content/application~1json/schema").cloned();
    let headers: Vec<(String, Value)> = resp
        .get("headers")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    view! {
        <div class="sv-resp">
            <div class="sv-resp-head">
                <span class="chip" class:err=is_default class:ok=!is_default>{status}</span>
                <span class="dim">{description}</span>
            </div>
            {schema.map(|s| components.with_value(|c| schema_view(&s, c, &[], false)))}
            {(!headers.is_empty())
                .then(|| {
                    view! {
                        <div class="sv-resp-headers">
                            <For each=move || headers.clone() key=|(n, _)| n.clone() let:h>
                                {
                                    let (name, def) = h;
                                    let desc =
                                        def.get("description").and_then(Value::as_str).unwrap_or_default().to_string();
                                    view! {
                                        <div class="sv-header-row">
                                            <code class="mono-sm">{name}</code>
                                            <span class="dim">{desc}</span>
                                        </div>
                                    }
                                }
                            </For>
                        </div>
                    }
                })}
        </div>
    }
}
