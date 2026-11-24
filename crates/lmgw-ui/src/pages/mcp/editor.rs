//! One MCP server's detail: the header with Test and a menu (Enable or
//! Disable, Delete), the hero with its live state, its form as preference
//! groups — the server, how it is started (stdio: the container or the
//! command, its arguments, environment and podman args) or reached (http,
//! sse: the URL and headers), how it behaves — and its tools as a boxed
//! list with their switches. Changed fields carry the accent dot, the save
//! toast counts them, Ctrl+S saves, and leaving it with changes asks first.
//! `/mcp-servers/new` is the same form, blank; its save moves to the row.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{McpServerSaved, McpServerView};

use super::form::{blank, McpForm};
use super::list::{endpoint, state_of};
use super::tools::{Band, Inventory};
use super::{Page, Sel};
use crate::fmt::count_of;
use crate::widgets::nav_split::{track_scroll, DetailHead, SaveKey};
use crate::widgets::{
    ActionRow, ComboRow, EntryRow, Ico, Icon, MenuItem, ModelPicker, NumberRow, PrefGroup,
    SaveToast, StateChip, SwitchRow, TextRow,
};

#[component]
pub(super) fn ServerDetail(
    page: Page,
    /// The saved server; `None` makes a new one.
    id: Option<i64>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    let toasts = page.toasts;
    let location = leptos_router::hooks::use_location();
    let row = Memo::new(move |_| id.and_then(|id| page.servers.by_id(id)));
    let form = McpForm::new(&row.get_untracked().unwrap_or_else(blank));
    let loaded = Memo::new(move |_| id.is_none() || row.with(Option::is_some));
    Effect::new(move |_| {
        if let Some(s) = row.get() {
            form.merge(&s);
        }
    });
    let dirty_count = form.dirty_count();
    let invalid = form.invalid();
    let here = id.map_or(Sel::New, Sel::Server);
    page.nav.guard().watch_in(
        "this server",
        Signal::derive(move || dirty_count.get() > 0),
        Signal::stored(here.path()),
    );

    let saving = RwSignal::new(false);
    let save_error = RwSignal::new(None::<String>);
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
            let res = crate::api::post::<McpServerSaved, _>("/api/op/mcp_server_set", &body).await;
            let _ = saving.try_set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v.message);
                    page.reload();
                    if form.dirty.try_get_untracked().is_none() {
                        return;
                    }
                    form.rebase(sent);
                    if target == 0 {
                        created.set(Some(v.id));
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
    Effect::new(move |_| {
        let Some(new_id) = created.get() else { return };
        let there = page
            .servers
            .rows
            .with(|r| r.as_ref().is_some_and(|r| r.iter().any(|s| s.id == new_id)));
        if there && dirty_count.get() == 0 {
            let q = super::kept_query(&location.search.get_untracked());
            page.nav
                .go_unasked(&format!("{}{q}", Sel::Server(new_id).path()), true);
        }
    });
    let on_discard = Callback::new(move |()| {
        form.discard();
        save_error.set(None);
    });
    SaveKey::install(on_save);

    let testing = RwSignal::new(false);
    let more = Signal::derive(move || {
        let Some(s) = row.get() else {
            return Vec::new();
        };
        let (id, enabled) = (s.id, s.enabled);
        let whole = StoredValue::new(s);
        vec![
            MenuItem::new(if enabled { "Disable" } else { "Enable" }, move || {
                page.op(if enabled { "disable" } else { "enable" }, id, None, None)
            })
            .title(if enabled {
                "Disconnect it and stop offering its tools; the configuration stays"
            } else {
                "Offer its tools again (it connects on first use, or now if autostart is on)"
            }),
            MenuItem::new("Delete…", move || {
                page.deleting.set(Some(whole.get_value()))
            })
            .destructive()
            .title("Remove it from the gateway; its tools leave /mcp"),
        ]
    });
    let title = Signal::derive(move || {
        row.get()
            .map(|s| s.name)
            .or_else(|| Some(form.text("name").get()).filter(|n| !n.trim().is_empty()))
            .unwrap_or_else(|| "New MCP server".into())
    });
    let sub = Signal::derive(move || {
        row.get()
            .map(|s| endpoint(&s))
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| "Not saved yet".into())
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
                                    "Connect now with the saved settings (save first to test the edits) and list its tools"
                                } else {
                                    "Connect now and list its tools"
                                }
                            }
                            on:click=move |_| page.op("test", id, Some(testing), None)
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
            <div class="detail-flow mcp-flow">
                <Hero page=page title=title sub=sub row=row/>
                <Show when=move || loaded.get() fallback=|| view! { <div class="mdetail-empty">"Loading…"</div> }>
                    {move || {
                        row.get()
                            .and_then(|s| {
                                let (status, _, detail) = page.status(&s);
                                (s.enabled && status == "error").then_some(detail).flatten()
                            })
                            .map(|d| {
                                view! {
                                    <PrefGroup title="Problem">
                                        <div class="prow problem-row">{d}</div>
                                    </PrefGroup>
                                }
                            })
                    }}
                    {move || {
                        row.get()
                            .and_then(|s| s.agent_id)
                            .map(|a| {
                                view! {
                                    <PrefGroup>
                                        <div class="prow notice-row">
                                            {format!("Registered by the agent '{a}': it writes this row when it is installed or updated.")}
                                        </div>
                                    </PrefGroup>
                                }
                            })
                    }}
                    <Groups form=form device=row.get_untracked().is_some_and(|s| s.transport == "device")/>
                    {id.map(|id| {
                        view! {
                            <Inventory inv=page.inv filter=page.filter only=Band::Source(String::new(), Some(id))/>
                        }
                    })}
                    {id.map(|_| {
                        view! {
                            <PrefGroup>
                                <ActionRow
                                    title="Delete server…"
                                    sub="Removes it from the gateway; its tools leave /mcp"
                                    danger=true
                                    on_click=Callback::new(move |()| {
                                        if let Some(s) = row.get_untracked() {
                                            page.deleting.set(Some(s));
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
            save_label=move || if id.is_none() && created.get().is_none() { "Add server" } else { "Save" }
        />
    }
}

/// The hero: the glyph, the name, the endpoint, and the live state's chips.
#[component]
fn Hero(
    page: Page,
    title: Signal<String>,
    sub: Signal<String>,
    row: Memo<Option<McpServerView>>,
) -> impl IntoView {
    let chips = move || {
        row.get().map(|s| {
            let (status, count, detail) = page.status(&s);
            let (dot, mut word) = state_of(s.enabled, &status);
            if s.enabled && status == "ready" {
                word = match page.offered(s.id) {
                    Some(n) => format!("Ready · {n}/{count} offered"),
                    None => format!("Ready · {}", count_of(count.max(0) as usize, "tools")),
                };
            }
            let transport_tip = (s.transport == "device").then_some(
                "a paired device's hosted tools: listed while its host link is open; its label and \
                 its name follow the device's hosting grant (Usage → Keys)",
            );
            view! {
                <StateChip dot=dot word=word title=detail.unwrap_or_default()/>
                {(s.open_tasks > 0).then(|| view! {
                    <span
                        class="chip info open-tasks"
                        title="MCP tasks this server runs for Chat threads now (no cap): each thread lists \
                               its own above the composer, with Cancel. A server with open tasks is never \
                               stopped for being idle. Counted when the list is read; a job that starts or \
                               ends reads it again."
                    >
                        {count_of(s.open_tasks as usize, "open tasks")}
                    </span>
                })}
                <span class="chip" title=transport_tip>{s.transport.clone()}</span>
                {(!s.tool_prefix.is_empty()).then(|| view! {
                    <span class="chip mono-chip" title="Its tools are offered under this prefix">
                        {format!("{}__", s.tool_prefix)}
                    </span>
                })}
                {s.agent_id.clone().map(|a| view! { <span class="chip">{format!("agent {a}")}</span> })}
            }
        })
    };
    view! {
        <div class="detail-hero">
            <div class="glyph">
                <Icon ico=Ico::Wrench/>
            </div>
            <div class="hero-text">
                <h1>{move || title.get()}</h1>
                <div
                    class=move || {
                        if sub.with(|s| s == "Not saved yet" || s.starts_with("device '")) { "sub" } else { "sub mono-sub" }
                    }
                    title=move || sub.get()
                >{move || sub.get()}</div>
                <div class="chips">{chips}</div>
            </div>
        </div>
    }
}

/// The form's groups. A device row's transport is its host link, and only
/// shown.
#[component]
fn Groups(form: McpForm, device: bool) -> impl IntoView {
    let transport = form.text("transport");
    let transport_opts = Signal::derive(move || {
        let all: &[(&str, &str)] = if device {
            &[("device", "Device (its own host link)")]
        } else {
            &[
                ("stdio", "stdio (a subprocess)"),
                ("http", "Streamable HTTP"),
                ("sse", "SSE"),
            ]
        };
        all.iter()
            .map(|(v, l)| (v.to_string(), l.to_string()))
            .collect::<Vec<_>>()
    });
    let stdio = move || transport.with(|t| t == "stdio");
    let t = move |k: &'static str| form.text(k);
    let d = move |k: &'static str| form.is_dirty(k);
    view! {
        <PrefGroup title="Server">
            <SwitchRow
                title="Enabled"
                sub="Its tools are offered on /mcp; off, it is disconnected and the configuration stays"
                value=form.enabled
                dirty=d("enabled")
            />
            <EntryRow title="Name" value=t("name") dirty=d("name")/>
            <ComboRow title="Transport" value=transport options=transport_opts dirty=d("transport")/>
        </PrefGroup>
        <Show when=stdio>
            <PrefGroup
                title="Process"
                desc="A stdio server is started by lmgw: in a container when an image is set (podman run -i), else as the bare command."
            >
                <EntryRow
                    title="Container image"
                    sub="Empty runs the bare command"
                    value=t("image")
                    mono=true
                    wide=true
                    dirty=d("image")
                />
                <EntryRow title="Command" value=t("command") mono=true wide=true dirty=d("command")/>
                <TextRow
                    mono=true
                    title="Command arguments"
                    sub="One per line, shell-quoted"
                    value=t("args")
                    dirty=d("args")
                />
                <TextRow
                    mono=true
                    title="Environment"
                    sub="KEY=VALUE per line; a server's credentials usually live here"
                    value=t("env")
                    dirty=d("env")
                />
                <EntryRow
                    title="Working directory"
                    value=t("cwd")
                    mono=true
                    wide=true
                    dirty=d("cwd")
                />
                <TextRow
                    mono=true
                    title="Extra podman run args"
                    sub="One argument per line; a bind mount needs :Z (or :z) under SELinux"
                    value=t("run_args")
                    dirty=d("run_args")
                />
            </PrefGroup>
        </Show>
        <Show when=move || !stdio()>
            <PrefGroup title="Connection">
                <EntryRow
                    title="URL"
                    value=t("url")
                    mono=true
                    wide=true
                    placeholder="https://mcp.example/mcp"
                    dirty=d("url")
                />
                <TextRow
                    mono=true
                    title="Headers"
                    sub="Name: Value per line; stored values show as <set>"
                    value=t("headers")
                    dirty=d("headers")
                />
            </PrefGroup>
        </Show>
        <PrefGroup title="Behaviour">
            <EntryRow
                title="Tool prefix"
                sub="Its tools are offered as prefix__tool; empty offers them by their own names"
                value=t("prefix")
                mono=true
                dirty=d("prefix")
            />
            <NumberRow
                title="Timeout"
                sub="How long one call may take"
                value=t("timeout")
                unit="ms"
                dirty=d("timeout")
                error=Signal::derive(move || form.timeout_error.get())
            />
            <NumberRow
                title="Stop when idle"
                sub="After this long without a call; 0 = never"
                value=t("idle")
                unit="s"
                dirty=d("idle")
                error=Signal::derive(move || form.idle_error.get())
            />
            <SwitchRow
                title="Autostart"
                sub="Connect when lmgw starts, not on first use"
                value=form.autostart
                dirty=d("autostart")
            />
            <SwitchRow
                title="Allow sampling"
                sub="The server may ask lmgw's models for completions"
                value=form.allow_sampling
                dirty=d("allow_sampling")
            />
            // The same choice Settings → Agents & tools makes for every
            // server, so the same picker: a typo here was a sampling route
            // that silently went nowhere.
            <div class="prow stack field-row" class:dirty=move || d("sampling_alias").get()>
                <div class="prow-text">
                    <span class="prow-title">"Sampling model"</span>
                    <span class="prow-sub">"A small dedicated model for its sampling requests"</span>
                </div>
                <div class="prow-end">
                    <ModelPicker
                        value=t("sampling_alias")
                        tasks=&["chat"]
                        empty_label="The global one (Settings → Agents & tools)"
                        recent_key="mcp-sampling"
                    />
                </div>
            </div>
        </PrefGroup>
    }
}
