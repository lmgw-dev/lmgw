//! `/api-reference` (api-docs design §6.1-6.3): fetches the document, owns
//! filter/selection/identity state and the per-operation draft cache, and
//! composes the rail, documentation and tester panes under `PageFrame`.

use std::collections::HashMap;

use leptos::prelude::*;
use serde_json::Value;

use lmgw_api_types::ConnectInfo;

use crate::scope::{Latest, Scope};
use crate::url_state::use_query_signal;
use crate::widgets::{filter_words, FilterBar, PageFrame, PageMode};

use super::detail::OperationDoc;
use super::doc::{ApiDoc, Operation};
use super::draft::Draft;
use super::identity::{held_caps, kind_label, Identity, KeyProbe};
use super::rail::{ApiRail, ApiRailSelect};
use super::send::{header_problem, revoke_blob};
use super::tester::{ActionBar, ApiTry, CurlPreview};

/// `GET /api/session` with a pasted key, never through `crate::api` (its
/// `401` handling locks the whole dashboard — §0.8, §6.7) and never with the
/// session cookie attached (`credentials: omit`). A non-`2xx` answer is a
/// matched row core refuses (a disabled key): `refused`.
async fn probe_key(key: &str) -> Option<KeyProbe> {
    let auth = format!("Bearer {key}");
    // gloo-net throws, rather than errs, on a header value it cannot carry.
    if header_problem("Authorization", &auth).is_some() {
        return None;
    }
    let req = gloo_net::http::Request::get("/api/session")
        .header("Authorization", &auth)
        .credentials(web_sys::RequestCredentials::Omit)
        .build()
        .ok()?;
    let resp = req.send().await.ok()?;
    if !resp.ok() {
        return Some(KeyProbe {
            refused: true,
            ..KeyProbe::default()
        });
    }
    let v: Value = resp.json().await.ok()?;
    Some(KeyProbe {
        authenticated: v
            .get("authenticated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        kind: v
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        name: v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        refused: false,
    })
}

#[component]
pub fn ApiReference() -> impl IntoView {
    let scope = Scope::new();
    // Drafts are made under this owner, not the selection view's (which a
    // switch of operation disposes): see `Draft`.
    let owner = Owner::current().expect("a component always renders under an owner");
    let doc = RwSignal::new(None::<ApiDoc>);
    let load_error = RwSignal::new(None::<String>);
    let auth_enabled = RwSignal::new(true);

    scope.spawn(async move {
        match crate::api::get::<Value>("/api/openapi.json").await {
            Ok(v) => doc.set(Some(ApiDoc::parse(&v))),
            Err(e) => load_error.set(Some(e.to_string())),
        }
    });
    scope.spawn(async move {
        if let Ok(c) = crate::api::get::<ConnectInfo>("/api/connect").await {
            auth_enabled.set(c.auth_enabled);
        }
    });

    let query = use_query_signal("q");
    let selected = use_query_signal("op");
    let group_facet = RwSignal::new(String::new());
    let method_facet = RwSignal::new(String::new());

    let identity = RwSignal::new(Identity::Session);
    // The pasted key, trimmed (a paste often brings a newline): page memory
    // only — never storage, the URL, a toast, the console or the curl.
    let key = RwSignal::new(String::new());
    let key_probe = RwSignal::new(None::<KeyProbe>);
    let checking_key = RwSignal::new(false);
    // Only the newest probe's answer lands: an older one for a key since
    // edited would label the new key with the old one's kind.
    let probe_seq = Latest::new();

    let do_check = move || {
        let k = key.get_untracked();
        let Some(ticket) = probe_seq.next() else {
            return;
        };
        if k.is_empty() {
            key_probe.set(None);
            checking_key.set(false);
            return;
        }
        checking_key.set(true);
        scope.spawn(async move {
            let probe = probe_key(&k).await;
            if probe_seq.is(ticket) {
                key_probe.set(probe);
                checking_key.set(false);
            }
        });
    };
    let check_key = Callback::new(move |()| do_check());

    // Per-operation request/response state, cached across a filter or
    // selection change so switching away and back keeps what was typed
    // (§6.3). Memory only — never persisted, so a pasted key never survives
    // a reload either.
    let drafts: StoredValue<HashMap<String, Draft>> = StoredValue::new(HashMap::new());
    // Leaving the page aborts every in-flight request and revokes every
    // Blob URL (§6.7). The drafts are this owner's own nodes, which are
    // disposed only after its cleanups have run.
    on_cleanup(move || {
        drafts.with_value(|m| {
            for d in m.values() {
                d.abort();
                if let Some(url) = d.blob_url() {
                    revoke_blob(&url);
                }
            }
        });
    });

    // The filtered, rail-ordered list (§6.4): recomputed only when the doc,
    // the query or the facets change, not on every unrelated signal.
    let filtered: Memo<Vec<Operation>> = Memo::new(move |_| {
        doc.with(|d| {
            let Some(d) = d else { return Vec::new() };
            let words = filter_words(&query.get());
            let group = group_facet.get();
            let method = method_facet.get();
            d.rail_order()
                .into_iter()
                .filter(|op| group.is_empty() || op.group == group)
                .filter(|op| method.is_empty() || op.method == method)
                .filter(|op| ApiDoc::matches(op, &words))
                .cloned()
                .collect()
        })
    });
    let total = Memo::new(move |_| doc.with(|d| d.as_ref().map_or(0, |d| d.operations.len())));

    let groups: Memo<Vec<(String, usize)>> = Memo::new(move |_| {
        doc.with(|d| {
            let Some(d) = d else { return Vec::new() };
            let mut out: Vec<(String, usize)> = Vec::new();
            for op in &d.operations {
                match out.iter_mut().find(|(g, _)| *g == op.group) {
                    Some((_, n)) => *n += 1,
                    None => out.push((op.group.clone(), 1)),
                }
            }
            out
        })
    });
    let methods: Memo<Vec<(String, usize)>> = Memo::new(move |_| {
        doc.with(|d| {
            let Some(d) = d else { return Vec::new() };
            let mut out: Vec<(String, usize)> = Vec::new();
            for op in &d.operations {
                match out.iter_mut().find(|(m, _)| *m == op.method) {
                    Some((_, n)) => *n += 1,
                    None => out.push((op.method.clone(), 1)),
                }
            }
            out
        })
    });

    let current_op: Memo<Option<Operation>> = Memo::new(move |_| {
        let id = selected.get();
        if id.is_empty() {
            return None;
        }
        doc.with(|d| {
            d.as_ref()
                .and_then(|d| d.operations.iter().find(|o| o.operation_id == id).cloned())
        })
    });

    let principals_for =
        move || doc.with(|d| d.as_ref().map(|d| d.principals.clone()).unwrap_or_default());
    let held = Signal::derive(move || {
        held_caps(
            identity.get(),
            &principals_for(),
            auth_enabled.get(),
            key_probe.get().as_ref(),
        )
    });

    view! {
        <PageFrame
            title="API reference"
            sub="lmgw's own HTTP API, read live from /api/openapi.json"
            mode=PageMode::Split
            class="api-ref"
            actions=move || {
                view! {
                    <div class="row api-identity">
                        <div class="facets" role="group" aria-label="Identity">
                            <button
                                type="button"
                                class="facet"
                                class:on=move || identity.get() == Identity::Session
                                on:click=move |_| identity.set(Identity::Session)
                            >
                                "Session"
                            </button>
                            <button
                                type="button"
                                class="facet"
                                class:on=move || identity.get() == Identity::Key
                                on:click=move |_| identity.set(Identity::Key)
                            >
                                "API key"
                            </button>
                            <button
                                type="button"
                                class="facet"
                                class:on=move || identity.get() == Identity::None
                                on:click=move |_| identity.set(Identity::None)
                            >
                                "No credential"
                            </button>
                        </div>
                        <Show when=move || identity.get() == Identity::Key>
                            <input
                                class="input mono"
                                type="password"
                                autocomplete="off"
                                spellcheck="false"
                                placeholder="pasted key — kept in memory only"
                                prop:value=move || key.get()
                                on:input=move |ev| {
                                    key.set(event_target_value(&ev).trim().to_string());
                                    // Whatever was probed was another key: drop its
                                    // answer, and any still on its way.
                                    let _ = probe_seq.next();
                                    key_probe.set(None);
                                    checking_key.set(false);
                                }
                                on:blur=move |_| do_check()
                            />
                            <button type="button" class="btn ghost sm" on:click=move |_| do_check()>
                                {move || if checking_key.get() { "Checking…" } else { "Check" }}
                            </button>
                            <span class="dim mono-sm">
                                {move || {
                                    if key.with(String::is_empty) {
                                        String::new()
                                    } else {
                                        kind_label(Identity::Key, key_probe.get().as_ref())
                                    }
                                }}
                            </span>
                        </Show>
                        <a class="btn ghost sm" href="/api/openapi.json" download="lmgw-openapi.json">
                            "Admin doc"
                        </a>
                        <a class="btn ghost sm" href="/v1/openapi.json" download="lmgw-openapi-v1.json">
                            "Inference doc"
                        </a>
                    </div>
                }
            }
            toolbar=move || {
                view! {
                    <FilterBar
                        query=query
                        placeholder="Filter by path, summary, op name or tag"
                        shown=Signal::derive(move || filtered.with(Vec::len))
                        total=Signal::derive(move || total.get())
                        noun="operations"
                        facets=crate::widgets::FacetSet {
                            items: Signal::derive(move || {
                                groups
                                    .get()
                                    .into_iter()
                                    .map(|(g, n)| crate::widgets::Facet { id: g.clone(), label: g, count: n })
                                    .collect()
                            }),
                            active: group_facet,
                        }
                        extra=move || {
                            view! {
                                <div class="facets" role="group" aria-label="Method">
                                    <button
                                        type="button"
                                        class="facet"
                                        class:on=move || method_facet.with(String::is_empty)
                                        on:click=move |_| method_facet.set(String::new())
                                    >
                                        "Any method"
                                    </button>
                                    <For each=move || methods.get() key=|(m, _)| m.clone() let:row>
                                        {
                                            let (m, n) = row;
                                            let pick = m.clone();
                                            view! {
                                                <button
                                                    type="button"
                                                    class="facet"
                                                    class:on=move || method_facet.with(|f| *f == m)
                                                    on:click=move |_| method_facet.set(pick.clone())
                                                >
                                                    {m.clone()} <span class="count">{n}</span>
                                                </button>
                                            }
                                        }
                                    </For>
                                </div>
                            }
                        }
                        on_clear=Callback::new(move |()| {
                            group_facet.set(String::new());
                            method_facet.set(String::new());
                        })
                    />
                }
            }
        >
            <ApiRail
                operations=Signal::derive(move || filtered.get())
                undocumented=Signal::derive(move || doc.with(|d| d.as_ref().map(|d| d.undocumented.clone()).unwrap_or_default()))
                selected=selected
            />
            <div class="split-pane">
                <ApiRailSelect operations=Signal::derive(move || filtered.get()) selected=selected/>
                {move || {
                    if let Some(e) = load_error.get() {
                        return view! { <div class="notice err">"Could not load the API description: " {e}</div> }
                            .into_any();
                    }
                    let Some(op) = current_op.get() else {
                        return view! {
                            <div class="notice">
                                {move || {
                                    if doc.with(Option::is_some) {
                                        "Pick an operation from the rail.".to_string()
                                    } else {
                                        "Loading…".to_string()
                                    }
                                }}
                            </div>
                        }
                            .into_any();
                    };
                    let components = doc.with_untracked(|d| d.as_ref().map(|d| d.components.clone()).unwrap_or(Value::Null));
                    let draft = drafts.try_update_value(|m| {
                        *m.entry(op.operation_id.clone())
                            .or_insert_with(|| owner.with(|| Draft::new(&op, &components, scope)))
                    });
                    let draft = match draft {
                        Some(d) => d,
                        None => return ().into_any(),
                    };
                    let doc_components = components.clone();
                    let components = StoredValue::new(components);
                    view! {
                        <ActionBar
                            op=op.clone()
                            draft=draft
                            components=components
                            identity=identity
                            key=key
                            key_probe=key_probe
                            held=held
                            check_key=check_key
                        />
                        <div class="fill-pane api-op">
                            <div class="api-op-grid">
                                <CurlPreview op=op.clone() draft=draft components=components identity=identity/>
                                <OperationDoc op=op.clone() components=doc_components/>
                                <ApiTry
                                    op=op
                                    draft=draft
                                    components=components
                                    identity=identity
                                    key_probe=key_probe
                                    held=held
                                />
                            </div>
                        </div>
                    }
                        .into_any()
                }}
            </div>
        </PageFrame>
    }
}
