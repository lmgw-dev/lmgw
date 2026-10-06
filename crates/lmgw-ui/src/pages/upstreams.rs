//! Upstreams — cloud/self-hosted providers the gateway routes to. Secrets
//! follow the house convention: empty API-key input keeps the stored key,
//! header values round-trip as "<set>".

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{UpstreamModelsResponse, UpstreamView, UpstreamsResponse};
use serde_json::{json, Value};

use super::models::ModelsData;
use crate::catalog::use_model_catalog;
use crate::fmt::grouped;
use crate::widgets::{
    use_toasts, MenuItem, Modal, ModalFooter, PageFrame, RowMenu, Select, Toasts,
};
use llama_facts::LlamaFactsRow;

mod llama_facts;

/// One upstream's live catalog on this page: how many models it lists, or
/// why it could not be read.
#[derive(Clone, PartialEq)]
enum Catalog {
    Loading,
    Ready(usize),
    Failed(String),
}

/// What the rows reach, captured at render: handlers run without an owner
/// to look context up in.
#[derive(Clone, Copy)]
struct Ctx {
    toasts: Toasts,
    list: RwSignal<Option<Result<Vec<UpstreamView>, String>>>,
    catalogs: RwSignal<HashMap<i64, Catalog>>,
    models: ModelsData,
    editing: RwSignal<Option<UpstreamView>>,
    deleting: RwSignal<Option<UpstreamView>>,
    refresh_catalog: Callback<()>,
}

impl Ctx {
    fn load(self) {
        spawn_local(async move {
            let res = crate::api::get::<UpstreamsResponse>("/api/upstreams").await;
            match res {
                Ok(r) => {
                    for u in r.upstreams.iter().filter(|u| u.enabled) {
                        self.count(u.id, None);
                    }
                    self.list.set(Some(Ok(r.upstreams)));
                }
                Err(e) => self.list.set(Some(Err(e.to_string()))),
            }
        });
        self.models.load();
    }

    /// Read one upstream's catalog size; with `tested`, toast the connection
    /// test's result with it.
    fn count(self, id: i64, tested: Option<String>) {
        self.catalogs.update(|m| {
            m.insert(id, Catalog::Loading);
        });
        spawn_local(async move {
            let res =
                crate::api::get::<UpstreamModelsResponse>(format!("/api/upstream-models?id={id}"))
                    .await;
            let state = match res {
                Ok(r) => Catalog::Ready(r.entries.len().max(r.models.len())),
                Err(e) => Catalog::Failed(e.to_string()),
            };
            if let Some(name) = tested {
                match &state {
                    Catalog::Ready(n) => self.toasts.ok(format!(
                        "{name}: connection OK · lists {} models",
                        grouped(*n as u64)
                    )),
                    Catalog::Failed(e) => self.toasts.warn(format!(
                        "{name}: connection OK, but its catalog did not load — {e}"
                    )),
                    Catalog::Loading => {}
                }
            }
            self.catalogs.update(|m| {
                m.insert(id, state);
            });
        });
    }

    /// Enable, disable or delete; what the gateway serves changes with it.
    fn set(self, action: &'static str, id: i64) {
        spawn_local(async move {
            match crate::api::post::<Value, _>(
                "/api/op/upstream_set",
                &json!({ "action": action, "id": id }),
            )
            .await
            {
                Ok(v) => {
                    self.toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("done")
                        .to_string());
                    self.refresh_catalog.try_run(());
                    self.load();
                }
                Err(e) => self.toasts.err(e.to_string()),
            }
        });
    }
}

#[component]
pub fn Upstreams() -> impl IntoView {
    let catalog = use_model_catalog();
    let ctx = Ctx {
        toasts: use_toasts(),
        list: RwSignal::new(None),
        catalogs: RwSignal::new(HashMap::new()),
        models: ModelsData::new(),
        editing: RwSignal::new(None),
        deleting: RwSignal::new(None),
        refresh_catalog: Callback::new(move |()| catalog.refresh()),
    };
    ctx.load();
    let editing = ctx.editing;
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = editing.get().is_some();
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && editing.get_untracked().is_some() {
            editing.set(None);
        }
    });
    let rows = Memo::new(move |_| {
        ctx.list
            .with(|l| l.as_ref().and_then(|r| r.as_ref().ok()).cloned())
            .unwrap_or_default()
    });

    view! {
        <PageFrame
            title="Upstreams"
            sub="where requests go"
            actions=move || {
                view! {
                    <button
                        class="btn primary"
                        on:click=move |_| {
                            editing
                                .set(
                                    Some(UpstreamView {
                                        timeout_ms: 120_000,
                                        enabled: true,
                                        protocol: "openai".into(),
                                        kind: "generic".into(),
                                        ..Default::default()
                                    }),
                                )
                        }
                    >
                        "Add upstream"
                    </button>
                }
            }
        >
            {move || match ctx.list.get() {
                None => Some(view! { <div class="card dim">"Loading…"</div> }.into_any()),
                Some(Err(e)) => {
                    Some(
                        view! {
                            <div class="notice err row">
                                "Loading upstreams failed: "
                                {e}
                                <button class="btn ghost sm" on:click=move |_| ctx.load()>
                                    "Retry"
                                </button>
                            </div>
                        }
                            .into_any(),
                    )
                }
                Some(Ok(r)) if r.is_empty() => {
                    Some(
                        view! {
                            <div class="card empty">
                                "No upstreams yet — add your first provider with Add upstream."
                            </div>
                        }
                            .into_any(),
                    )
                }
                Some(Ok(_)) => None,
            }}
            <Show when=move || !rows.with(Vec::is_empty)>
                <div class="card pad0">
                    <table class="data">
                        <thead>
                            <tr>
                                <th>"Name"</th>
                                <th class="col-p3">"Protocol"</th>
                                <th>"Base URL"</th>
                                <th class="col-p2">"Key"</th>
                                <th class="col-p2">"Passthrough"</th>
                                <th title="What its live model list holds; opens in Models → Upstream catalogs">
                                    "Catalog"
                                </th>
                                <th>"State"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            <For each=move || rows.get() key=|u| format!("{u:?}") let:u>
                                <UpRow u=u ctx=ctx/>
                            </For>
                        </tbody>
                    </table>
                </div>
            </Show>

            <Modal open=open title="Upstream" guard=true>
                {move || {
                    editing
                        .get()
                        .map(|u| {
                            view! {
                                <UpstreamForm
                                    u=u
                                    open=open
                                    on_saved=move || {
                                        ctx.refresh_catalog.run(());
                                        ctx.load();
                                    }
                                />
                            }
                                .into_any()
                        })
                }}
            </Modal>
            <DeleteModal ctx=ctx/>
        </PageFrame>
    }
}

#[component]
fn UpRow(u: UpstreamView, ctx: Ctx) -> impl IntoView {
    let id = u.id;
    let enabled = u.enabled;
    let testing = RwSignal::new(false);
    let name = StoredValue::new(u.name.clone());
    let test = move |_| {
        if testing.get_untracked() {
            return;
        }
        testing.set(true);
        // Read now: the row can be re-rendered while the test is out.
        let name = name.get_value();
        spawn_local(async move {
            let res =
                crate::api::post::<Value, _>("/api/op/upstream_test", &json!({ "id": id })).await;
            testing.set(false);
            match res {
                // The count comes with the OK: a connection that works but
                // lists nothing is worth knowing about.
                Ok(_) => ctx.count(id, Some(name)),
                Err(e) => ctx.toasts.err(e.to_string()),
            }
        });
    };
    let passthrough = if u.expose_all {
        if u.expose_prefix.is_empty() {
            "*".to_string()
        } else {
            format!("{}/*", u.expose_prefix)
        }
    } else {
        "–".to_string()
    };
    let expose_all = u.expose_all;
    let catalog_cell = move || {
        if !enabled {
            return view! { <span class="dim" title="A disabled upstream is not asked">"—"</span> }
                .into_any();
        }
        let hidden = ctx.models.full.with(|f| {
            f.as_ref()
                .and_then(|f| f.passthrough.iter().find(|p| p.id == id))
                .map(|p| p.hidden.len())
        });
        match ctx.catalogs.with(|m| m.get(&id).cloned()) {
            None | Some(Catalog::Loading) => view! { <span class="dim">"counting…"</span> }.into_any(),
            Some(Catalog::Failed(e)) => {
                // Nothing answering is said in one word; the server keeps
                // reqwest's detail after it, which the tooltip carries.
                let text = if e.starts_with("unreachable") {
                    "unreachable".to_string()
                } else {
                    e.clone()
                };
                view! { <span class="problem" title=e>{text}</span> }.into_any()
            }
            Some(Catalog::Ready(n)) if expose_all => {
                let href = format!(
                    "/models/catalog?upstream={}",
                    String::from(js_sys::encode_uri_component(&name.get_value()))
                );
                let hidden = hidden.unwrap_or(0);
                view! {
                    <a class="row-link" href=href title="Open it in Models → Upstream catalogs">
                        {format!("{} models · {} hidden", grouped(n as u64), grouped(hidden as u64))}
                    </a>
                }
                .into_any()
            }
            Some(Catalog::Ready(n)) => view! {
                <span class="dim" title="Not exposed as a whole catalog: its models are reachable through aliases">
                    {format!("{} models · aliases only", grouped(n as u64))}
                </span>
            }
            .into_any(),
        }
    };
    let edit_u = StoredValue::new(u.clone());
    // A llama.cpp row's server, per model, in a row of its own under it.
    let llama = (u.protocol == LLAMA_CPP).then(|| {
        view! { <LlamaFactsRow facts=u.llama_facts.clone() enabled=enabled span=8/> }
    });
    let menu = Signal::derive(move || {
        vec![
            MenuItem::new("Edit", move || ctx.editing.set(Some(edit_u.get_value()))),
            MenuItem::new(if enabled { "Disable" } else { "Enable" }, move || {
                ctx.set(if enabled { "disable" } else { "enable" }, id)
            })
            .title(if enabled {
                "Stop routing to it; its aliases and catalog go quiet until it is enabled again"
            } else {
                "Route to it again"
            }),
            MenuItem::new("Delete…", move || {
                ctx.deleting.set(Some(edit_u.get_value()))
            })
            .title("Remove it and its aliases"),
        ]
    });
    view! {
        <tr class:muted=!enabled>
            <td>
                <button
                    class="link-btn row-link"
                    title="Edit"
                    on:click=move |_| ctx.editing.set(Some(edit_u.get_value()))
                >
                    {u.name.clone()}
                </button>
            </td>
            <td class="col-p3"><span class="type-badge">{u.protocol.clone()}</span></td>
            <td class="clip dim mono-sm" title=u.base_url.clone()>{u.base_url.clone()}</td>
            <td class="col-p2 dim">{if u.has_api_key { "•••• set" } else { "none" }}</td>
            <td class="col-p2 dim mono-sm">{passthrough}</td>
            <td class="up-catalog">{catalog_cell}</td>
            <td>
                <span class=if enabled { "chip ok" } else { "chip off" }>
                    <span class="dot"></span>
                    {if enabled { "enabled" } else { "disabled" }}
                </span>
            </td>
            <td class="actions">
                <button
                    class="btn sm"
                    disabled=move || testing.get()
                    title="Ask it for its model list: checks the URL, the key and the protocol"
                    on:click=test
                >
                    {move || if testing.get() { "Testing…" } else { "Test" }}
                </button>
                " "
                <RowMenu items=menu/>
            </td>
        </tr>
        {llama}
    }
}

/// Deleting an upstream takes its aliases with it: the question says how
/// many, from the configured models.
#[component]
fn DeleteModal(ctx: Ctx) -> impl IntoView {
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = ctx.deleting.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && ctx.deleting.with_untracked(Option::is_some) {
            ctx.deleting.set(None);
        }
    });
    view! {
        <Modal open=open title="Delete upstream">
            {move || {
                ctx.deleting
                    .get()
                    .map(|u| {
                        let id = u.id;
                        let aliases: Vec<String> = ctx.models.full.with(|f| {
                            f.as_ref()
                                .map(|f| {
                                    f.aliases
                                        .iter()
                                        .filter(|a| a.upstream_id == id)
                                        .map(|a| a.alias.clone())
                                        .collect()
                                })
                                .unwrap_or_default()
                        });
                        let what = match aliases.len() {
                            0 => "No alias uses it.".to_string(),
                            1 => format!("Its alias {} is removed with it.", aliases[0]),
                            n => format!("Its {n} aliases are removed with it: {}.", aliases.join(", ")),
                        };
                        view! {
                            <p>"Delete '" {u.name.clone()} "'? " {what}</p>
                            <ModalFooter>
                                <button class="btn ghost" on:click=move |_| open.set(false)>
                                    "Keep it"
                                </button>
                                <button
                                    class="btn danger"
                                    on:click=move |_| {
                                        open.set(false);
                                        ctx.set("delete", id);
                                    }
                                >
                                    "Delete"
                                </button>
                            </ModalFooter>
                        }
                    })
            }}
        </Modal>
    }
}

#[component]
fn UpstreamForm(
    u: UpstreamView,
    open: RwSignal<bool>,
    on_saved: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let toasts = use_toasts();
    let create = u.id == 0;
    let id = u.id;
    let name = RwSignal::new(u.name.clone());
    let protocol = RwSignal::new(u.protocol.clone());
    let kind = RwSignal::new(u.kind.clone());
    let base_url = RwSignal::new(u.base_url.clone());
    let api_key = RwSignal::new(String::new());
    let has_key = u.has_api_key;
    let headers = RwSignal::new(
        u.extra_headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let timeout = RwSignal::new(u.timeout_ms.to_string());
    let expose_all = RwSignal::new(u.expose_all);
    let expose_prefix = RwSignal::new(u.expose_prefix.clone());
    let supports_responses = RwSignal::new(u.supports_responses);
    let enabled = RwSignal::new(u.enabled);
    let saving = RwSignal::new(false);

    // The kind follows the protocol into and out of llama.cpp (design §5):
    // the form always posts `kind`, so without the reset a switch away from
    // llama_cpp would come back as the old openai + llama_server spelling.
    Effect::new(move |prev: Option<String>| {
        let p = protocol.get();
        if let Some(prev) = prev {
            let k = kind_after_protocol(&prev, &p, &kind.get_untracked());
            if k != kind.get_untracked() {
                kind.set(k);
            }
        }
        p
    });
    let llama_cpp = move || protocol.with(|p| p == LLAMA_CPP);

    let protocol_opts = Signal::derive(|| {
        [
            ("openai", "OpenAI-compatible"),
            ("anthropic", "Anthropic"),
            ("gemini", "Gemini"),
            (LLAMA_CPP, "llama.cpp (llama-server, ik_llama.cpp)"),
        ]
        .into_iter()
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect::<Vec<_>>()
    });
    // llama-server is a kind of the Anthropic protocol only: llama.cpp's own
    // server is the llama_cpp protocol (design §5).
    let kind_opts = Signal::derive(move || {
        let anthropic = protocol.with(|p| p == "anthropic");
        [
            ("generic", "generic"),
            ("llama_server", "llama-server"),
            ("audio_cpp", "audio.cpp"),
        ]
        .into_iter()
        .filter(|(v, _)| anthropic || *v != "llama_server")
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect::<Vec<_>>()
    });

    let save = move |_| {
        if saving.get_untracked() {
            return;
        }
        let mut parsed_headers: Vec<(String, String)> = Vec::new();
        for line in headers.get_untracked().lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match line.split_once(':') {
                Some((k, v)) => parsed_headers.push((k.trim().into(), v.trim().into())),
                None => {
                    toasts.err(format!("header line '{line}' is not 'Name: Value'"));
                    return;
                }
            }
        }
        let timeout_v: u64 = match timeout.get_untracked().trim().parse() {
            Ok(v) => v,
            Err(_) => {
                toasts.err("timeout must be a number of milliseconds");
                return;
            }
        };
        let body = json!({
            "action": if create { "create" } else { "update" },
            "id": if create { Value::Null } else { json!(id) },
            "name": name.get_untracked().trim(),
            "protocol": protocol.get_untracked(),
            "kind": kind.get_untracked(),
            "base_url": base_url.get_untracked().trim(),
            "api_key": api_key.get_untracked(),
            "extra_headers": parsed_headers,
            "timeout_ms": timeout_v,
            "expose_all": expose_all.get_untracked(),
            "expose_prefix": expose_prefix.get_untracked().trim(),
            // Never on for llama_cpp (decision 19); the checkbox is hidden there.
            "supports_responses": supports_responses.get_untracked()
                && protocol.with_untracked(|p| p != LLAMA_CPP),
            "enabled": enabled.get_untracked(),
        });
        saving.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/op/upstream_set_full", &body).await;
            saving.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("saved")
                        .to_string());
                    open.set(false);
                    on_saved();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    view! {
        <div class="spec-grid">
            <div class="field">
                <label>"Name"</label>
                <input class="input" prop:value=move || name.get()
                    on:input=move |ev| name.set(event_target_value(&ev))/>
            </div>
            <div class="field">
                <label>"Protocol"</label>
                <Select value=protocol options=protocol_opts/>
            </div>
            <div class="field" style="grid-column:1 / -1">
                <label>"Base URL"</label>
                <input class="input mono" style="width:100%" prop:value=move || base_url.get()
                    on:input=move |ev| base_url.set(event_target_value(&ev))/>
            </div>
            <div class="field">
                <label>
                    {if has_key { "API key · stored — empty keeps it" } else { "API key" }}
                </label>
                <input class="input mono" type="password"
                    placeholder=if has_key { "(unchanged)" } else { "" }
                    prop:value=move || api_key.get()
                    on:input=move |ev| api_key.set(event_target_value(&ev))/>
            </div>
            <div class="field">
                <label>"Timeout · ms"</label>
                <input class="input mono" prop:value=move || timeout.get()
                    on:input=move |ev| timeout.set(event_target_value(&ev))/>
            </div>
            // llama_cpp is always a llama-server: no kind to pick.
            {move || {
                (!llama_cpp())
                    .then(|| {
                        view! {
                            <div class="field">
                                <label>"Kind"</label>
                                <Select value=kind options=kind_opts/>
                            </div>
                        }
                    })
            }}
            <div class="field">
                <label>"Passthrough prefix"</label>
                <input class="input mono" prop:value=move || expose_prefix.get()
                    on:input=move |ev| expose_prefix.set(event_target_value(&ev))/>
            </div>
        </div>
        <div class="field" style="margin-top:8px">
            <label>"Extra headers (Name: Value per line; stored values show as <set>)"</label>
            <textarea class="input mono ta" prop:value=move || headers.get()
                on:input=move |ev| headers.set(event_target_value(&ev))></textarea>
        </div>
        <div class="row" style="margin-top:10px">
            <label class="row dim" style="gap:5px">
                <input type="checkbox" prop:checked=move || expose_all.get()
                    on:change=move |ev| expose_all.set(event_target_checked(&ev))/>
                "expose the whole catalog (passthrough)"
            </label>
            {move || {
                (!llama_cpp())
                    .then(|| {
                        view! {
                            <label class="row dim" style="gap:5px">
                                <input type="checkbox" prop:checked=move || supports_responses.get()
                                    on:change=move |ev| supports_responses.set(event_target_checked(&ev))/>
                                "native /v1/responses"
                            </label>
                        }
                    })
            }}
            <label class="row dim" style="gap:5px">
                <input type="checkbox" prop:checked=move || enabled.get()
                    on:change=move |ev| enabled.set(event_target_checked(&ev))/>
                "enabled"
            </label>
        </div>
        <ModalFooter>
            <button class="btn ghost" on:click=move |_| open.set(false)>
                "Cancel"
            </button>
            <button class="btn primary" disabled=move || saving.get() on:click=save>
                {move || if saving.get() { "Saving…" } else { "Save upstream" }}
            </button>
        </ModalFooter>
    }
}

/// The protocol value of llama.cpp's own server (llama-server, ik_llama.cpp).
const LLAMA_CPP: &str = "llama_cpp";

/// The kind once the protocol moved from `prev` to `next`: `llama_cpp` is
/// always `llama_server`, leaving it resets the kind to `generic`, and
/// `llama_server` survives only under `anthropic` (llama.cpp egress design §5,
/// what `settle` accepts). Any other move keeps the owner's pick.
fn kind_after_protocol(prev: &str, next: &str, kind: &str) -> String {
    if next == LLAMA_CPP {
        "llama_server".into()
    } else if prev == LLAMA_CPP || (next != "anthropic" && kind == "llama_server") {
        "generic".into()
    } else {
        kind.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_follows_the_protocol_into_and_out_of_llama_cpp() {
        assert_eq!(
            kind_after_protocol("openai", "llama_cpp", "generic"),
            "llama_server"
        );
        assert_eq!(
            kind_after_protocol("anthropic", "llama_cpp", "audio_cpp"),
            "llama_server"
        );
        // Leaving llama_cpp: never the old openai + llama_server spelling.
        assert_eq!(
            kind_after_protocol("llama_cpp", "openai", "llama_server"),
            "generic"
        );
        assert_eq!(
            kind_after_protocol("llama_cpp", "anthropic", "llama_server"),
            "generic"
        );
        // Elsewhere the owner's pick stands.
        assert_eq!(
            kind_after_protocol("openai", "anthropic", "llama_server"),
            "llama_server"
        );
        // llama_server only survives under anthropic.
        assert_eq!(
            kind_after_protocol("anthropic", "gemini", "llama_server"),
            "generic"
        );
        assert_eq!(
            kind_after_protocol("anthropic", "openai", "llama_server"),
            "generic"
        );
        assert_eq!(
            kind_after_protocol("anthropic", "openai", "audio_cpp"),
            "audio_cpp"
        );
        assert_eq!(
            kind_after_protocol("anthropic", "anthropic", "llama_server"),
            "llama_server"
        );
        assert_eq!(
            kind_after_protocol("openai", "gemini", "audio_cpp"),
            "audio_cpp"
        );
    }
}
