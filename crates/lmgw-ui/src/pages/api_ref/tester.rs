//! The "try" column and its action bar (api-docs design §6.6, §6.7, §6.9):
//! the param/header/body editors, the model picker integration, the Send
//! and Stop buttons, "Copy as curl" and its preview, and the live response.
//! The state they edit, and the request built from it, is `draft.rs`; the
//! finished response is rendered by `response_view.rs`.

use std::collections::HashMap;

use leptos::prelude::*;
use serde_json::Value;

use crate::widgets::schema_form::task_scope;
use crate::widgets::{ConfirmButton, ModelPicker, Select};

use super::doc::Operation;
use super::draft::{
    blocker, body_schema, curl_command_for, multipart_fields, send_request, Draft, ModelSlot,
};
use super::identity::{expectation, kind_label, Identity, KeyProbe};
use super::response_view::{timing_line, HeadersTable, ResponseBody, StatusChip};

fn location_origin() -> String {
    window().location().origin().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Action bar (§6.2 `div.action-bar.api-op-bar`)
// ---------------------------------------------------------------------------

#[component]
pub fn ActionBar(
    op: Operation,
    draft: Draft,
    components: StoredValue<Value>,
    #[prop(into)] identity: Signal<Identity>,
    #[prop(into)] key: Signal<String>,
    #[prop(into)] key_probe: Signal<Option<KeyProbe>>,
    #[prop(into)] held: Signal<Vec<String>>,
    /// Probe a pasted key nobody checked yet (§6.5 "automatically before
    /// the first send") — for the expectation chip; the send itself does
    /// not wait for it.
    check_key: Callback<()>,
) -> impl IntoView {
    let method = op.method.clone();
    let path = op.path.clone();
    let capability = op.capability.clone();
    let writes = op.writes;
    let reveals = op.reveals_secret;
    let deprecated = op.deprecated;
    // Review R3 #8: what the document already says about the operation's
    // relatives — the self-admin tool sharing its code path, and the op a
    // deprecated alias stands for.
    let tool = op.tool.clone();
    let alias_of = op.alias_of.clone();
    let untyped = op.untyped_reason().map(str::to_string);
    let confirm_text = format!(
        "Send {method} {path}?{}",
        op.confirm_note
            .as_deref()
            .map(|n| format!(" {n}"))
            .unwrap_or_default()
    );

    let cap_warn = Signal::derive({
        let capability = capability.clone();
        move || {
            expectation(
                &held.get(),
                &capability,
                identity.get(),
                key_probe.get().as_ref(),
            )
        }
    });
    let blocked = Signal::derive({
        let op = op.clone();
        move || blocker(&op, draft, identity.get(), &key.get())
    });
    let cannot_send =
        Signal::derive(move || draft.in_flight.get() || blocked.with(Option::is_some));

    let op_for_send = op.clone();
    let do_send = move || {
        let identity = identity.get_untracked();
        if identity == Identity::Key && key_probe.get_untracked().is_none() {
            check_key.run(());
        }
        components
            .with_value(|c| send_request(&op_for_send, draft, c, identity, key.get_untracked()));
    };

    // §6.9: the button copies (the `CopyBtn` clipboard call) and shows the
    // command below the bar; the label says so for two seconds.
    let copied = RwSignal::new(false);
    let copy_curl = move |_| {
        let text = components.with_value(|c| {
            curl_command_for(&op, draft, c, identity.get_untracked(), &location_origin())
        });
        let _ = window().navigator().clipboard().write_text(&text);
        draft.show_curl.set(true);
        copied.set(true);
        set_timeout(move || copied.set(false), std::time::Duration::from_secs(2));
    };

    view! {
        <div class="action-bar api-op-bar">
            <span class=format!("method m-{}", method.to_ascii_lowercase())>{method.clone()}</span>
            <code class="mono-sm api-op-path">{path.clone()}</code>
            <span class="chip">{capability.clone()}</span>
            {writes.then(|| view! { <span class="chip warn">"writes"</span> })}
            {reveals.then(|| view! { <span class="chip warn">"reveals a secret"</span> })}
            {deprecated.then(|| view! { <span class="chip err">"deprecated"</span> })}
            {alias_of.map(|a| {
                let title = format!("A deprecated alias: send {a} instead — same arguments, same answer.");
                view! { <span class="chip off" title=title>"alias of " {a}</span> }
            })}
            {tool.map(|t| {
                let title = format!("Same code path as the self-admin MCP tool {t}, served on /mcp/admin.");
                view! { <span class="chip off" title=title>"same as MCP tool " {t}</span> }
            })}
            {untyped.clone().map(|w| view! { <span class="chip off" title=w>"untyped"</span> })}
            {move || cap_warn.get().map(|w| view! { <span class="chip warn cap-warn" title=w.clone()>{w.clone()}</span> })}
            {move || blocked.get().map(|b| view! { <span class="chip off api-blocked" title=b.clone()>{b.clone()}</span> })}
            <span class="spacer"></span>
            <button type="button" class="btn ghost sm" on:click=copy_curl>
                {move || if copied.get() { "Copied ✓" } else { "Copy as curl" }}
            </button>
            <Show when=move || draft.in_flight.get()>
                <button type="button" class="btn ghost sm" on:click=move |_| draft.abort()>
                    "Stop"
                </button>
            </Show>
            {if writes || reveals {
                view! {
                    <ConfirmButton
                        label="Send"
                        confirm=confirm_text.clone()
                        class="btn primary"
                        disabled=cannot_send
                        on_confirm=Callback::new(move |()| do_send())
                    />
                }
                    .into_any()
            } else {
                view! {
                    <button
                        type="button"
                        class="btn primary tester-send"
                        disabled=move || cannot_send.get()
                        on:click=move |_| do_send()
                    >
                        {move || if draft.in_flight.get() { "Sending…" } else { "Send" }}
                    </button>
                }
                    .into_any()
            }}
        </div>
    }
}

/// `pre.curl-preview` (§6.9) — inside the one scroller, above the grid, so
/// a long body neither pushes the pane off screen nor scrolls on its own.
/// Follows the editors while it is open.
#[component]
pub fn CurlPreview(
    op: Operation,
    draft: Draft,
    components: StoredValue<Value>,
    #[prop(into)] identity: Signal<Identity>,
) -> impl IntoView {
    view! {
        <Show when=move || draft.show_curl.get()>
            {
                let op = op.clone();
                view! {
                    <div class="curl-preview-wrap">
                        <pre class="curl-preview">
                            {move || {
                                components
                                    .with_value(|c| {
                                        curl_command_for(&op, draft, c, identity.get(), &location_origin())
                                    })
                            }}
                        </pre>
                        <button type="button" class="link-btn" on:click=move |_| draft.show_curl.set(false)>
                            "hide"
                        </button>
                    </div>
                }
            }
        </Show>
    }
}

// ---------------------------------------------------------------------------
// The try column (§6.2 `section.api-try`)
// ---------------------------------------------------------------------------

fn params_in(op: &Operation, location: &str) -> Vec<Value> {
    op.parameters()
        .iter()
        .filter(|p| p.get("in").and_then(Value::as_str) == Some(location))
        .cloned()
        .collect()
}

#[component]
pub fn ApiTry(
    op: Operation,
    draft: Draft,
    components: StoredValue<Value>,
    #[prop(into)] identity: Signal<Identity>,
    #[prop(into)] key_probe: Signal<Option<KeyProbe>>,
    #[prop(into)] held: Signal<Vec<String>>,
) -> impl IntoView {
    let notice = Signal::derive({
        let capability = op.capability.clone();
        move || {
            let label = kind_label(identity.get(), key_probe.get().as_ref());
            let held_line = held.get().join(", ");
            let warn = expectation(
                &held.get(),
                &capability,
                identity.get(),
                key_probe.get().as_ref(),
            );
            (label, held_line, warn)
        }
    });

    let path_params = params_in(&op, "path");
    let query_params = params_in(&op, "query");
    // §6.6: agent-audience headers fold under "More headers".
    let (agent_headers, headers): (Vec<Value>, Vec<Value>) =
        params_in(&op, "header").into_iter().partition(|p| {
            p.get(lmgw_api_types::openapi_ext::AUDIENCE)
                .and_then(Value::as_str)
                == Some(lmgw_api_types::openapi_ext::audience::AGENT)
        });

    let request_body = components.with_value(|c| body_schema(&op, c));
    let model_slot = components.with_value(|c| ModelSlot::for_op(&op, c));
    let model_picker = model_slot.map(|slot| model_field(slot, draft, op.model_task.as_deref()));

    view! {
        <section class="api-try">
            <div class="notice sv-identity">
                {move || {
                    let (label, held_line, warn) = notice.get();
                    view! {
                        <div>"Sending as " <b>{label}</b> {(!held_line.is_empty()).then(|| format!(" — holds: {held_line}"))}</div>
                        {warn.map(|w| view! { <div class="field-warn">{w}</div> })}
                    }
                }}
            </div>

            {param_grid(path_params, draft.path_params)}
            {param_grid(query_params, draft.query_params)}
            {param_grid(headers, draft.header_params)}
            {(!agent_headers.is_empty())
                .then(|| {
                    view! {
                        <details class="explain">
                            <summary>"More headers (agent containers)"</summary>
                            {param_grid(agent_headers, draft.header_params)}
                        </details>
                    }
                })}
            <div class="field">
                <label>"Extra headers" <span class="field-unit">"Name: value, one per line"</span></label>
                <textarea
                    class="input mono ta"
                    rows=3
                    prop:value=move || draft.extra_headers.get()
                    on:input=move |ev| draft.extra_headers.set(event_target_value(&ev))
                ></textarea>
            </div>

            {model_picker}

            {request_body
                .map(|(mime, schema)| {
                    if mime == "application/json" {
                        json_body_view(&op, draft, components)
                    } else if mime == "multipart/form-data" {
                        components.with_value(|c| multipart_fields_view(&schema, c, draft))
                    } else {
                        raw_body_view(draft)
                    }
                })}

            <ResponsePanel draft=draft/>
        </section>
    }
}

/// The `ModelPicker` bound both ways to where this op keeps its model
/// (§6.6): a pick writes the slot, and an edit of the slot (typing into the
/// JSON body, the multipart field or the query param) moves the picker.
/// Neither direction writes when the two already agree, so opening an op
/// never reformats a body the user typed.
fn model_field(slot: ModelSlot, draft: Draft, task: Option<&str>) -> AnyView {
    let model_value = RwSignal::new(untrack(|| slot.read(draft)).unwrap_or_default());
    Effect::new(move |_| {
        if let Some(m) = slot.read(draft) {
            if m != model_value.get_untracked() {
                model_value.set(m);
            }
        }
    });
    Effect::new(move |_| {
        let m = model_value.get();
        if m.is_empty() || untrack(|| slot.read(draft)).as_deref() == Some(m.as_str()) {
            return;
        }
        slot.write(draft, &m);
    });
    let json_unusable = Signal::derive(move || {
        slot == ModelSlot::Json
            && draft.body_text.with(|t| {
                serde_json::from_str::<Value>(t)
                    .ok()
                    .is_none_or(|v| !v.is_object())
            })
    });
    view! {
        <div class="field">
            <label>
                "Model"
                <Show when=move || json_unusable.get()>
                    <span class="field-unit">"fix the JSON body to pick a model"</span>
                </Show>
            </label>
            <ModelPicker value=model_value tasks=task_scope(task) allow_custom=true disabled=json_unusable/>
        </div>
    }
    .into_any()
}

fn json_body_view(op: &Operation, draft: Draft, components: StoredValue<Value>) -> AnyView {
    let schema = op
        .request_body_schema()
        .map(|(_, s)| s.clone())
        .unwrap_or_default();
    let body_valid = Signal::derive(move || {
        draft
            .body_text
            .with(|t| t.trim().is_empty() || serde_json::from_str::<Value>(t).is_ok())
    });
    view! {
        <div class="field">
            <label>
                "Body"
                <span class="field-unit">"application/json"</span>
                <button
                    type="button"
                    class="link-btn"
                    on:click=move |_| {
                        let ex = components.with_value(|c| super::example::body_example(&schema, c));
                        draft.body_text.set(serde_json::to_string_pretty(&ex).unwrap_or_default());
                    }
                >
                    "Reset to example"
                </button>
            </label>
            <textarea
                class="input mono ta"
                rows=move || draft.body_text.with(|t| t.lines().count().max(4) as i32)
                prop:value=move || draft.body_text.get()
                on:input=move |ev| draft.body_text.set(event_target_value(&ev))
            ></textarea>
            <Show when=move || !body_valid.get()>
                <p class="field-warn">"not valid JSON"</p>
            </Show>
        </div>
    }
    .into_any()
}

fn param_grid(params: Vec<Value>, values: RwSignal<HashMap<String, String>>) -> Option<AnyView> {
    (!params.is_empty()).then(|| {
        view! {
            <div class="field-grid">
                {params.into_iter().map(|p| param_input(p, values)).collect_view()}
            </div>
        }
        .into_any()
    })
}

/// A path/query/header parameter's editor (§6.6): a `Select` for an enum,
/// a tri-state unset/true/false `Select` for a boolean, a text input
/// otherwise. Empty means omitted.
fn param_input(p: Value, values: RwSignal<HashMap<String, String>>) -> AnyView {
    let name = p
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let required = p.get("required").and_then(Value::as_bool).unwrap_or(false);
    let description = p
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let schema = p.get("schema").cloned().unwrap_or(Value::Null);
    let choices: Option<Vec<String>> = match schema.get("enum").and_then(Value::as_array) {
        Some(vals) => Some(
            vals.iter()
                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
                .collect(),
        ),
        None if schema.get("type").and_then(Value::as_str) == Some("boolean") => {
            Some(vec!["true".to_string(), "false".to_string()])
        }
        None => None,
    };
    let label = view! {
        <label title=description>{name.clone()} {required.then_some(" *")}</label>
    };
    let control = match choices {
        Some(choices) => {
            let local =
                RwSignal::new(values.with_untracked(|m| m.get(&name).cloned().unwrap_or_default()));
            let key = name.clone();
            Effect::new(move |_| {
                let v = local.get();
                values.maybe_update(|m| {
                    if m.get(&key).map_or("", String::as_str) == v {
                        return false;
                    }
                    m.insert(key.clone(), v);
                    true
                });
            });
            let options = std::iter::once((String::new(), "unset".to_string()))
                .chain(choices.into_iter().map(|c| (c.clone(), c)))
                .collect::<Vec<_>>();
            view! { <Select value=local options=Signal::derive(move || options.clone()) placeholder="unset"/> }
                .into_any()
        }
        None => {
            let key = name.clone();
            let key2 = name.clone();
            view! {
                <input
                    class="input mono"
                    prop:value=move || values.with(|m| m.get(&key).cloned().unwrap_or_default())
                    on:input=move |ev| {
                        values.update(|m| {
                            m.insert(key2.clone(), event_target_value(&ev));
                        })
                    }
                />
            }
            .into_any()
        }
    };
    view! { <div class="field">{label} {control}</div> }.into_any()
}

/// Every file an `<input type=file>` holds now.
fn picked_files(ev: &web_sys::Event) -> Vec<web_sys::File> {
    let el: web_sys::HtmlInputElement = event_target(ev);
    let Some(list) = el.files() else {
        return Vec::new();
    };
    (0..list.length()).filter_map(|i| list.get(i)).collect()
}

fn set_files(draft: Draft, field: &str, files: Vec<web_sys::File>) {
    draft.files.update(|m| {
        if files.is_empty() {
            m.remove(field);
        } else {
            m.insert(field.to_string(), files);
        }
    });
}

/// What a file field holds, by name — shown beside the input, which a
/// switch away and back re-creates empty while the draft keeps the files.
fn picked_names(draft: Draft, field: String) -> impl IntoView {
    move || {
        draft.files.with(|m| {
            m.get(&field).map(|fs| {
                let names = fs
                    .iter()
                    .map(web_sys::File::name)
                    .collect::<Vec<_>>()
                    .join(", ");
                view! { <span class="dim mono-sm">{names}</span> }
            })
        })
    }
}

/// One field per multipart property (§6.6): a file input for a property with
/// `contentMediaType` (`multiple` for an array), a text input otherwise.
fn multipart_fields_view(schema: &Value, components: &Value, draft: Draft) -> AnyView {
    let rows = multipart_fields(schema, components)
        .into_iter()
        .map(|f| {
            let name = f.name.clone();
            if f.is_file {
                let field = name.clone();
                view! {
                    <div class="field">
                        <label>{name.clone()}</label>
                        <input
                            type="file"
                            class="input"
                            multiple=f.multiple
                            on:change=move |ev| set_files(draft, &field, picked_files(&ev))
                        />
                        {picked_names(draft, name.clone())}
                    </div>
                }
                .into_any()
            } else {
                let field = name.clone();
                let field2 = name.clone();
                view! {
                    <div class="field">
                        <label>{name.clone()}</label>
                        <input
                            class="input"
                            prop:value=move || {
                                draft.multipart_fields.with(|m| m.get(&field).cloned().unwrap_or_default())
                            }
                            on:input=move |ev| {
                                draft.multipart_fields.update(|m| {
                                    m.insert(field2.clone(), event_target_value(&ev));
                                })
                            }
                        />
                    </div>
                }
                .into_any()
            }
        })
        .collect_view();
    view! { <div class="field-grid">{rows}</div> }.into_any()
}

/// A raw (`Req::Raw`) body: one file input, plus the content-type field it
/// defaults to the file's own `type` (§6.6).
fn raw_body_view(draft: Draft) -> AnyView {
    view! {
        <div class="field-grid">
            <div class="field">
                <label>"File"</label>
                <input
                    type="file"
                    class="input"
                    on:change=move |ev| {
                        let files = picked_files(&ev);
                        if let Some(f) = files.first() {
                            if draft.raw_content_type.with_untracked(String::is_empty) {
                                draft.raw_content_type.set(f.type_());
                            }
                        }
                        set_files(draft, "file", files);
                    }
                />
                {picked_names(draft, "file".to_string())}
            </div>
            <div class="field">
                <label>"Content-Type"</label>
                <input
                    class="input mono"
                    prop:value=move || draft.raw_content_type.get()
                    on:input=move |ev| draft.raw_content_type.set(event_target_value(&ev))
                />
            </div>
        </div>
    }
    .into_any()
}

#[component]
fn ResponsePanel(draft: Draft) -> impl IntoView {
    view! {
        <div class="resp-panel">
            {move || {
                draft
                    .finished
                    .get()
                    .map(|f| {
                        view! {
                            <div class="resp-view">
                                <div class="row">
                                    <StatusChip status=f.status/>
                                    <span class="dim">{timing_line(f.headers_ms, f.done_ms)}</span>
                                </div>
                                <HeadersTable headers=f.headers/>
                                <ResponseBody outcome=f.outcome_kind/>
                            </div>
                        }
                    })
            }}
            <Show when=move || !draft.sse_frames.with(Vec::is_empty)>
                <div class="resp-sse">
                    // Keyed by index, read on render: the list only ever grows
                    // (a new send clears it first), and handing `For` the
                    // whole frame list on every batch would copy all of it.
                    <For each=move || 0..draft.sse_frames.with(Vec::len) key=|i| *i let:i>
                        {
                            let f = draft.sse_frames.with_untracked(|f| f.get(i).cloned()).unwrap_or_default();
                            view! {
                                <div class="frame">
                                    <span class="dim">{f.event}</span>
                                    <pre>{f.data}</pre>
                                </div>
                            }
                        }
                    </For>
                    <Show when=move || !draft.sse_text.with(String::is_empty)>
                        <pre class="assembled">{move || draft.sse_text.get()}</pre>
                    </Show>
                </div>
            </Show>
        </div>
    }
}
