//! One upstream's detail: the header with Test and a menu (Enable or
//! Disable, its catalog, its prices, Delete), the hero, what lmgw remembers
//! of it (out of quota, a llama.cpp server's facts), and the form as
//! preference groups — connection, authentication, limits, catalog,
//! pricing. Changed fields carry the accent dot, the save toast counts them,
//! Ctrl+S saves, and leaving it with changes asks first. `/upstreams/new`
//! is the same form, blank; its save moves to the row it made.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{RowSaved, UpstreamView};

use super::form::{blank, UpForm};
use super::hero::Hero;
use super::llama_facts::LlamaGroup;
use super::quota::QuotaGroup;
use super::{Page, Sel, LLAMA_CPP, OPENAI, OPENAI_COMPATIBLE, PROTOCOLS};
use crate::widgets::nav_split::{track_scroll, DetailHead, SaveKey};
use crate::widgets::{
    ActionRow, ComboRow, EntryRow, MenuItem, NumberRow, PrefGroup, SaveToast, SwitchRow, TextRow,
};

#[component]
pub(super) fn UpDetail(
    page: Page,
    /// The saved row; `None` makes a new one.
    id: Option<i64>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    let toasts = page.toasts;
    let location = leptos_router::hooks::use_location();
    let row = Memo::new(move |_| id.and_then(|id| page.by_id(id)));
    let form = UpForm::new(&row.get_untracked().unwrap_or_else(blank));
    // A deep link reads the list first: the form shows once it has the row.
    let loaded = Memo::new(move |_| id.is_none() || row.with(Option::is_some));
    // The row as the gateway has it now: unchanged values follow it.
    Effect::new(move |_| {
        if let Some(u) = row.get() {
            form.merge(&u);
        }
    });
    let dirty_count = form.dirty_count();
    let invalid = form.invalid();
    let here = id.map_or(Sel::New, Sel::Row);
    page.nav.guard().watch_in(
        "this upstream",
        Signal::derive(move || dirty_count.get() > 0),
        Signal::stored(here.path()),
    );

    let saving = RwSignal::new(false);
    let save_error = RwSignal::new(None::<String>);
    // What a create made: later saves update it, and the page moves to it.
    let created = RwSignal::new(None::<i64>);
    let on_save = Callback::new(move |()| {
        if saving.get_untracked() || invalid.get_untracked() > 0 || dirty_count.get_untracked() == 0
        {
            return;
        }
        let target = id.or(created.get_untracked()).unwrap_or(0);
        let sent = form.snapshot();
        let body = match form.body(target) {
            Ok(b) => b,
            Err(e) => {
                save_error.set(Some(e));
                return;
            }
        };
        saving.set(true);
        save_error.set(None);
        spawn_local(async move {
            let res = crate::api::post::<RowSaved, _>("/api/op/upstream_set_full", &body).await;
            let _ = saving.try_set(false);
            match res {
                Ok(saved) => {
                    toasts.ok(saved.message);
                    // The same note Test shows, as a warning of its own.
                    if let Some(note) = saved.note {
                        toasts.warn(note);
                    }
                    // A new prefix rewrites the fallbacks naming its models.
                    for note in saved.fallback_notes {
                        toasts.warn(note);
                    }
                    page.refresh_catalog.try_run(());
                    // The URL or the key may be another now: its catalog is
                    // asked again.
                    // (a disabled one is not asked)
                    if sent[11] == "true" {
                        page.count(saved.id, None);
                    }
                    page.load();
                    if form.dirty.try_get_untracked().is_none() {
                        return;
                    }
                    form.rebase(sent);
                    if target == 0 {
                        created.set(Some(saved.id));
                    }
                }
                Err(e) => {
                    let e = e.to_string();
                    toasts.err(format!("Not saved: {e}"));
                    let _ = save_error.try_set(Some(e));
                }
            }
        });
    });
    // A create moves to its row once the list has it, while the form is
    // clean (edits typed after the create stay here; their save updates it).
    Effect::new(move |_| {
        let Some(new_id) = created.get() else { return };
        let there = page
            .list
            .with(|l| l.as_ref().is_some_and(|l| l.iter().any(|u| u.id == new_id)));
        if there && dirty_count.get() == 0 {
            let q = super::list_query(&location.search.get_untracked());
            page.nav
                .go_unasked(&format!("{}{q}", Sel::Row(new_id).path()), true);
        }
    });
    let on_discard = Callback::new(move |()| {
        form.discard();
        save_error.set(None);
    });
    SaveKey::install(on_save);

    let testing = RwSignal::new(false);
    let saved_name = Signal::derive(move || row.get().map(|u| u.name).unwrap_or_default());
    let more = Signal::derive(move || {
        let Some(u) = row.get() else {
            return Vec::new();
        };
        let id = u.id;
        let mut v = vec![
            MenuItem::new(if u.enabled { "Disable" } else { "Enable" }, move || {
                page.set(if u.enabled { "disable" } else { "enable" }, id)
            })
            .title(if u.enabled {
                "Stop routing to it; its aliases and catalog go quiet until it is enabled again"
            } else {
                "Route to it again"
            }),
        ];
        if u.expose_all {
            let href = catalog_href(&u.name);
            v.push(
                MenuItem::new("Open its catalog", move || {
                    page.nav.go(&href, false);
                })
                .title("Every model it exposes, in Models → Upstream catalogs"),
            );
        }
        let prices = prices_href(&u.name);
        v.push(MenuItem::new("Prices", move || {
            page.nav.go(&prices, false);
        }));
        let whole = StoredValue::new(u);
        v.push(
            MenuItem::new("Delete…", move || {
                page.deleting.set(Some(whole.get_value()))
            })
            .destructive()
            .title("Remove it and its aliases"),
        );
        v
    });

    let title = Signal::derive(move || {
        row.get()
            .map(|u| u.name)
            .or_else(|| Some(form.name.get()).filter(|n| !n.trim().is_empty()))
            .unwrap_or_else(|| "New upstream".into())
    });
    let sub = Signal::derive(move || {
        let url = row
            .get()
            .map(|u| u.base_url)
            .unwrap_or_else(|| form.base_url.get());
        if url.trim().is_empty() {
            "No base URL yet".to_string()
        } else {
            url
        }
    });
    let scrolled = RwSignal::new(false);

    view! {
        <DetailHead
            narrow=narrow
            back=back
            title=title
            scrolled=scrolled
            actions=move || {
                id.map(|id| {
                    view! {
                        <button
                            type="button"
                            class="btn"
                            disabled=move || testing.get()
                            title=move || {
                                if dirty_count.get() > 0 {
                                    "Ask it for its model list with the saved settings (save first to test the edits): checks the URL, the key and the protocol"
                                } else {
                                    "Ask it for its model list: checks the URL, the key and the protocol"
                                }
                            }
                            on:click=move |_| page.test(id, saved_name.get_untracked(), testing)
                        >
                            {move || if testing.get() { "Testing…" } else { "Test" }}
                        </button>
                    }
                })
            }
            more=more
        />
        <div
            class="detail-scroll mdetail-scroll"
            tabindex="-1"
            on:scroll=move |ev| track_scroll(&ev, scrolled)
        >
            <div class="detail-flow">
                <Hero page=page name=title sub=sub row=row/>
                <Show when=move || loaded.get() fallback=|| view! { <div class="mdetail-empty">"Loading…"</div> }>
                    // Remembered out of quota (openai egress design §14.6).
                    {move || {
                        row.get()
                            .and_then(|u| u.quota.map(|q| (u.id, q)))
                            .map(|(id, q)| {
                                view! {
                                    <QuotaGroup
                                        id=id
                                        quota=q
                                        toasts=toasts
                                        cleared=Callback::new(move |()| page.load())
                                    />
                                }
                            })
                    }}
                    // A llama.cpp row's server, per model.
                    <Show when=move || row.with(|r| r.as_ref().is_some_and(|u| u.protocol == LLAMA_CPP))>
                        <LlamaGroup facts=Signal::derive(move || {
                            row.get().map(|u| u.llama_facts).unwrap_or_default()
                        })/>
                    </Show>
                    <Groups page=page form=form row=row/>
                    {id.map(|_| {
                        view! {
                            <PrefGroup>
                                <ActionRow
                                    title="Delete upstream…"
                                    sub="Removes it and the aliases that use it"
                                    danger=true
                                    on_click=Callback::new(move |()| {
                                        if let Some(u) = row.get_untracked() {
                                            page.deleting.set(Some(u));
                                        }
                                    })
                                />
                            </PrefGroup>
                        }
                    })}
                </Show>
            </div>
        </div>
        <SaveToast
            dirty_count=dirty_count
            invalid=invalid
            saving=saving
            error=save_error
            on_save=on_save
            on_discard=on_discard
            save_label=move || if id.is_none() && created.get().is_none() { "Add upstream" } else { "Save" }
        />
    }
}

fn catalog_href(name: &str) -> String {
    format!(
        "/models/catalog?upstream={}",
        String::from(js_sys::encode_uri_component(name))
    )
}

fn prices_href(name: &str) -> String {
    format!(
        "/usage/prices?q={}",
        String::from(js_sys::encode_uri_component(name))
    )
}

/// The form's groups.
#[component]
fn Groups(page: Page, form: UpForm, row: Memo<Option<UpstreamView>>) -> impl IntoView {
    let protocol_opts = Signal::derive(|| {
        PROTOCOLS
            .iter()
            .map(|(v, l)| (v.to_string(), l.to_string()))
            .collect::<Vec<_>>()
    });
    let protocol = form.protocol;
    // llama-server is a kind of the Anthropic protocol only: llama.cpp's own
    // server is the llama_cpp protocol (llama egress design §5). An external
    // audio.cpp server is OpenAI-compatible (openai egress design §1.5).
    let kind_opts = Signal::derive(move || {
        let anthropic = protocol.with(|p| p == "anthropic");
        let compatible = protocol.with(|p| p == OPENAI_COMPATIBLE);
        [
            ("generic", "Generic"),
            ("llama_server", "llama-server"),
            ("audio_cpp", "audio.cpp"),
        ]
        .into_iter()
        .filter(|(v, _)| anthropic || *v != "llama_server")
        .filter(|(v, _)| compatible || *v != "audio_cpp")
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect::<Vec<_>>()
    });
    let llama_cpp = move || protocol.with(|p| p == LLAMA_CPP);
    let platform = move || protocol.with(|p| p == OPENAI);
    let compatible = move || protocol.with(|p| p == OPENAI_COMPATIBLE);
    let has_key = move || row.with(|r| r.as_ref().is_some_and(|u| u.has_api_key));
    let key_dirty = form.is_dirty("api_key");
    let saved_catalog = move || {
        row.with(|r| {
            r.as_ref()
                .filter(|u| u.expose_all)
                .map(|u| catalog_href(&u.name))
        })
    };
    view! {
        <PrefGroup title="Connection">
            <SwitchRow
                title="Enabled"
                sub="Requests are routed to it; off, its aliases and catalog go quiet"
                value=form.enabled
                dirty=form.is_dirty("enabled")
            />
            <EntryRow title="Name" value=form.name dirty=form.is_dirty("name")/>
            <ComboRow
                title="Protocol"
                sub=move || {
                    match protocol.get().as_str() {
                        LLAMA_CPP => "llama.cpp's own server: llama-server or ik_llama.cpp",
                        OPENAI => "The OpenAI platform, api.openai.com",
                        OPENAI_COMPATIBLE => "Any other provider with OpenAI's API shape",
                        _ => "",
                    }
                }
                value=form.protocol
                options=protocol_opts
                dirty=form.is_dirty("protocol")
            />
            // llama_cpp is always a llama-server and openai always generic: no
            // kind to pick.
            <Show when=move || !llama_cpp() && !platform()>
                <ComboRow
                    title="Kind"
                    sub="What answers at the URL, when it is a server lmgw knows"
                    value=form.kind
                    options=kind_opts
                    dirty=form.is_dirty("kind")
                />
            </Show>
            <EntryRow
                title="Base URL"
                value=form.base_url
                mono=true
                wide=true
                placeholder="https://api.example/v1"
                dirty=form.is_dirty("base_url")
            />
        </PrefGroup>
        <PrefGroup title="Authentication">
            <div class="prow entry" class:dirty=move || key_dirty.get()>
                <div class="prow-text">
                    <span class="prow-title">"API key"</span>
                    <span class="prow-sub">
                        {move || {
                            if has_key() {
                                "Stored: empty keeps it, a key typed here replaces it"
                            } else {
                                "None stored"
                            }
                        }}
                    </span>
                </div>
                <div class="prow-end">
                    <input
                        class="prow-entry mono"
                        type="password"
                        autocomplete="off"
                        spellcheck="false"
                        aria-label="API key"
                        placeholder=move || if has_key() { "(unchanged)" } else { "" }
                        prop:value=move || form.api_key.get()
                        on:input=move |ev| form.api_key.set(event_target_value(&ev))
                    />
                </div>
            </div>
            <TextRow
                mono=true
                title="Extra headers"
                sub="Name: Value per line; stored values show as <set>"
                value=form.headers
                dirty=form.is_dirty("headers")
                error=Signal::derive(move || form.headers_error.get())
            />
        </PrefGroup>
        <PrefGroup title="Limits">
            <NumberRow
                title="Timeout"
                sub="How long lmgw waits for an answer to start, and, once a streamed answer has begun, how long it may go silent before lmgw ends it with an error. 0 = no limit."
                value=form.timeout
                unit="ms"
                dirty=form.is_dirty("timeout")
                error=Signal::derive(move || form.timeout_error.get())
            />
        </PrefGroup>
        <PrefGroup title="Catalog">
            <SwitchRow
                title="Expose the whole catalog"
                sub="Passthrough: every model it lists can be requested as prefix/model, without an alias"
                value=form.expose_all
                dirty=form.is_dirty("expose_all")
            />
            <EntryRow
                title="Passthrough prefix"
                sub="Empty requests its models by their own ids"
                value=form.expose_prefix
                mono=true
                dirty=form.is_dirty("expose_prefix")
            />
            // OpenRouter lists its non-text models (decisions, embeddings,
            // speech, images…) only when asked; Gemini's OpenAI-compatible
            // endpoint refuses the question (decision-models design §6.2).
            <Show when=compatible>
                <SwitchRow
                    title="List every output modality"
                    sub="Asks GET …/models?output_modalities=all; off for an endpoint that refuses it, such as Gemini's OpenAI-compatible one"
                    value=form.catalog_all_modalities
                    dirty=form.is_dirty("catalog_all_modalities")
                />
                <SwitchRow
                    title="Native /v1/responses"
                    sub="Forwards /v1/responses to it as it is, instead of answering it from /v1/chat/completions"
                    value=form.supports_responses
                    dirty=form.is_dirty("supports_responses")
                />
            </Show>
            <Show when=platform>
                <ActionRow
                    title="Native /v1/responses"
                    sub="Always on the OpenAI platform. Requests with MCP tools lmgw serves, or continuing a response lmgw stored, stay in lmgw's own loop."
                />
            </Show>
            {move || {
                saved_catalog()
                    .map(|href| {
                        view! {
                            <ActionRow
                                title="Open its catalog"
                                sub="Every model it exposes, in Models → Upstream catalogs"
                                chevron=true
                                on_click=Callback::new(move |()| {
                                    page.nav.go(&href, false);
                                })
                            />
                        }
                    })
            }}
        </PrefGroup>
        {move || {
            row.get()
                .map(|u| {
                    let href = prices_href(&u.name);
                    view! {
                        <PrefGroup title="Pricing">
                            <ActionRow
                                title="Prices"
                                sub="What each unit of its models costs: the prices its catalog advertises, and the rows set by hand (Usage → Prices)"
                                chevron=true
                                on_click=Callback::new(move |()| {
                                    page.nav.go(&href, false);
                                })
                            />
                        </PrefGroup>
                    }
                })
        }}
    }
}
