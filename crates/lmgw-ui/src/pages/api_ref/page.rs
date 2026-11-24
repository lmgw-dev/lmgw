//! `/api-reference/:op?` (api-docs design §6.1-6.3): fetches the document,
//! owns filter/selection/identity state and the per-operation draft cache,
//! and composes the operations list beside the opened operation on the
//! shared split view (`widgets::nav_split`): its documentation and tester.
//! The operation is the path (`/api-reference/post_v1_chat_completions`);
//! the old `?op=` link lands there.

use std::collections::HashMap;

use leptos::prelude::*;
use serde_json::Value;

use lmgw_api_types::{ConnectInfo, SessionView};

use leptos_router::hooks::{use_location, use_navigate, use_params_map};

use crate::scope::{Latest, Scope};
use crate::url_state::use_query_signal;
use crate::widgets::list_pane::{query_without, seg, ListNav, ListRefs};
use crate::widgets::nav_split::{
    provide_save_key, track_scroll, DetailHead, NavSplit, SplitConf, SplitState,
};
use crate::widgets::{filter_words, Ico, Icon, MenuItem, PrefGroup};

use super::detail::OperationDoc;
use super::doc::{ApiDoc, Operation};
use super::draft::Draft;
use super::identity::{held_caps, kind_label, Identity, KeyProbe};
use super::list::{op_label, Facets, OpsPane};
use super::send::{header_problem, revoke_blob};
use super::tester::{ActionBar, ApiTry, CurlPreview};

const CONF: SplitConf = SplitConf {
    // Dense: the tester's fields are the older controls, which take their
    // height from the page's density.
    class: "api-split density-dense",
    width_key: "api_ref.list.width",
    list_width: (260, 28, 380),
    detail_min: 480,
    narrow_below: 800.0,
    handle_label: "Resize the list",
};

/// Save a document the server serves (the admin or the inference
/// description) under a name, as a link with `download` would.
fn download(href: &str, name: &str) {
    let Ok(a) = document().create_element("a") else {
        return;
    };
    let _ = a.set_attribute("href", href);
    let _ = a.set_attribute("download", name);
    if let Ok(a) = wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(a) {
        a.click();
    }
}

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
    let v: SessionView = resp.json().await.ok()?;
    Some(KeyProbe {
        authenticated: v.authenticated,
        kind: v.kind,
        name: v.name,
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
    let params = use_params_map();
    let location = use_location();
    let navigate = use_navigate();
    let sel = Memo::new(move |_| {
        params
            .with(|p| p.get("op"))
            .filter(|s| !s.trim().is_empty())
    });
    let go = StoredValue::new(move |to: String, replace: bool| {
        navigate(
            &to,
            leptos_router::NavigateOptions {
                replace,
                scroll: false,
                ..Default::default()
            },
        )
    });
    // The old link: `?op=<id>` opens the operation at its path.
    let old = use_query_signal("op");
    Effect::new(move |_| {
        let id = old.get();
        if !id.is_empty() {
            let rest = query_without(&location.search.get_untracked(), &["op"]);
            go.with_value(|g| g(format!("/api-reference/{}{rest}", seg(&id)), true));
        }
    });
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
        let id = sel.get()?;
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

    let split = SplitState::new(CONF);
    let narrow = split.narrow;
    let save_key = provide_save_key();
    let has_sel = Memo::new(move |_| sel.with(Option::is_some));
    let open_row = Callback::new(move |(id, replace): (String, bool)| {
        if sel.get_untracked().as_deref() != Some(id.as_str()) {
            let rest = query_without(&location.search.get_untracked(), &["op"]);
            go.with_value(|g| g(format!("/api-reference/{}{rest}", seg(&id)), replace));
        }
    });
    let order = Signal::derive(move || {
        filtered.with(|ops| {
            ops.iter()
                .map(|o| o.operation_id.clone())
                .collect::<Vec<_>>()
        })
    });
    let nav = ListNav::new(ListRefs::new(), order, sel, narrow, open_row, |id| {
        format!("/api-reference/{id}")
    });
    let back = Callback::new(move |()| {
        let rest = query_without(&location.search.get_untracked(), &["op"]);
        go.with_value(|g| g(format!("/api-reference{rest}"), false));
        request_animation_frame(move || nav.focus_list());
    });
    let facets = Facets {
        group: group_facet,
        method: method_facet,
        groups,
        methods,
    };
    let downloads = Signal::derive(|| {
        vec![
            MenuItem::new("Save the admin document", || {
                download("/api/openapi.json", "lmgw-openapi.json")
            })
            .title("/api/openapi.json: the dashboard's and the ops' API"),
            MenuItem::new("Save the inference document", || {
                download("/v1/openapi.json", "lmgw-openapi-v1.json")
            })
            .title("/v1/openapi.json: the OpenAI- and Anthropic-shaped routes"),
        ]
    });

    // Who the tester sends as: the session, a pasted key, or nothing.
    let identity_bar = move || {
        view! {
            <div class="api-identity">
                <span class="api-identity-label">"Send as"</span>
                <div class="seg" role="group" aria-label="Identity">
                    <button
                        type="button"
                        class="seg-btn"
                        class:active=move || identity.get() == Identity::Session
                        on:click=move |_| identity.set(Identity::Session)
                    >
                        "Session"
                    </button>
                    <button
                        type="button"
                        class="seg-btn"
                        class:active=move || identity.get() == Identity::Key
                        on:click=move |_| identity.set(Identity::Key)
                    >
                        "API key"
                    </button>
                    <button
                        type="button"
                        class="seg-btn"
                        class:active=move || identity.get() == Identity::None
                        on:click=move |_| identity.set(Identity::None)
                    >
                        "No credential"
                    </button>
                </div>
                <Show when=move || identity.get() == Identity::Key>
                    <input
                        class="input mono api-key"
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
                    <button type="button" class="btn sm" on:click=move |_| do_check()>
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
            </div>
        }
    };

    let detail = ViewFn::from(move || {
        let owner = owner.clone();
        let scrolled = RwSignal::new(false);
        let title = Signal::derive(move || {
            current_op
                .get()
                .map(|op| op_label(&op))
                .unwrap_or_else(|| "API reference".to_string())
        });
        view! {
            {move || {
                // The head is built with the operation: its controls are the
                // operation's own, and the head and the scroller under it
                // are replaced together.
                let head = move |actions: Option<ViewFn>| match actions {
                    Some(actions) => view! {
                        <DetailHead
                            narrow=narrow
                            back=back
                            back_label="Back to the operations"
                            title=title
                            scrolled=scrolled
                            actions=actions
                            more=downloads
                        />
                    }
                        .into_any(),
                    None => view! {
                        <DetailHead
                            narrow=narrow
                            back=back
                            back_label="Back to the operations"
                            title=title
                            scrolled=scrolled
                            more=downloads
                        />
                    }
                        .into_any(),
                };
                if let Some(e) = load_error.get() {
                    return view! {
                        {head(None)}
                        <div class="detail-scroll mdetail-scroll">
                            <div class="notice err api-load-err">"Could not load the API description: " {e}</div>
                        </div>
                    }
                        .into_any();
                }
                let Some(op) = current_op.get() else {
                    return view! {
                        {head(None)}
                        <div
                            class="detail-scroll mdetail-scroll"
                            tabindex="-1"
                            on:scroll=move |ev| track_scroll(&ev, scrolled)
                        >
                            <ApiStatus
                                loaded=Signal::derive(move || doc.with(Option::is_some))
                                undocumented=Signal::derive(move || {
                                    doc.with(|d| d.as_ref().map(|d| d.undocumented.clone()).unwrap_or_default())
                                })
                                total=Signal::derive(move || total.get())
                            />
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
                let method = op.method.clone();
                let summary = op.summary.clone();
                let bar_op = op.clone();
                let actions = ViewFn::from(move || {
                    view! {
                        <ActionBar
                            op=bar_op.clone()
                            draft=draft
                            components=components
                            identity=identity
                            key=key
                            key_probe=key_probe
                            held=held
                            check_key=check_key
                        />
                    }
                });
                view! {
                    {head(Some(actions))}
                    <div
                        class="detail-scroll mdetail-scroll api-op"
                        tabindex="-1"
                        on:scroll=move |ev| track_scroll(&ev, scrolled)
                    >
                        <div class="api-flow">
                            <div class="detail-hero api-hero">
                                <span class=format!("method api-hero-method m-{}", method.to_ascii_lowercase())>
                                    {method.clone()}
                                </span>
                                <div class="hero-text">
                                    <h1>{op_label(&op)}</h1>
                                    {(!summary.is_empty()).then(|| view! { <div class="sub">{summary.clone()}</div> })}
                                </div>
                            </div>
                            {identity_bar()}
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
                    </div>
                }
                    .into_any()
            }}
        }
    });

    view! {
        <NavSplit
            split=split
            has_sel=has_sel
            save_key=save_key
            search=nav.search
            search_off=Signal::derive(move || narrow.get() && has_sel.get())
            list=ViewFn::from(move || {
                view! {
                    <OpsPane
                        nav=nav
                        narrow=narrow
                        query=query
                        operations=Signal::derive(move || filtered.get())
                        total=Signal::derive(move || total.get())
                        loaded=Signal::derive(move || doc.with(Option::is_some) || load_error.with(Option::is_some))
                        facets=facets
                    />
                }
            })
            detail=detail
        />
    }
}

/// Nothing opened: what the page is, the two documents, and the routes it
/// does not describe.
#[component]
fn ApiStatus(
    loaded: Signal<bool>,
    undocumented: Signal<Vec<super::doc::Undocumented>>,
    total: Signal<usize>,
) -> impl IntoView {
    view! {
        <div class="detail-flow api-status">
            <div class="detail-hero">
                <div class="glyph"><Icon ico=Ico::Book/></div>
                <div class="hero-text">
                    <h1>"API reference"</h1>
                    <div class="sub">
                        {move || {
                            if loaded.get() {
                                format!(
                                    "lmgw's own HTTP API, read live from /api/openapi.json · {}",
                                    crate::fmt::count_of(total.get(), "operations"),
                                )
                            } else {
                                "Loading…".to_string()
                            }
                        }}
                    </div>
                    <div class="sub">"Pick an operation to read it and try it."</div>
                </div>
            </div>
            <PrefGroup title="Documents">
                <a class="prow action" href="/api/openapi.json" download="lmgw-openapi.json">
                    <div class="prow-text">
                        <span class="prow-title">"Admin document"</span>
                        <span class="prow-sub mono">"/api/openapi.json"</span>
                    </div>
                    <Icon ico=Ico::Download class="prow-chevron"/>
                </a>
                <a class="prow action" href="/v1/openapi.json" download="lmgw-openapi-v1.json">
                    <div class="prow-text">
                        <span class="prow-title">"Inference document"</span>
                        <span class="prow-sub mono">"/v1/openapi.json"</span>
                    </div>
                    <Icon ico=Ico::Download class="prow-chevron"/>
                </a>
            </PrefGroup>
            <Show when=move || !undocumented.with(Vec::is_empty)>
                <PrefGroup title="Not documented here">
                    <For each=move || undocumented.get() key=|u| format!("{} {}", u.method, u.path) let:u>
                        <div class="prow" title=u.reason.clone()>
                            <div class="prow-text">
                                <span class="prow-title mono">{format!("{} {}", u.method, u.path)}</span>
                                <span class="prow-sub">{u.reason.clone()}</span>
                            </div>
                        </div>
                    </For>
                </PrefGroup>
            </Show>
        </div>
    }
}
