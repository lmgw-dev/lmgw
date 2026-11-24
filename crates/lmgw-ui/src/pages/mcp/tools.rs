//! The tool inventory: everything the gateway serves northbound, per tool,
//! with the owner's switch (it applies on the click). One boxed group per
//! source — a server, or a built-in toolset (`lmgw__*`, `docs__*`), which
//! appear nowhere else in the dashboard — and the stale records at the end.
//!
//! With nothing selected the detail pane shows all of it, each source's
//! group folded until asked for (an inventory can run to a hundred tools or
//! more, most of them from one server); a selected source shows its own group, open. Either way the
//! filter above (`?q=` name or description, `?show=` a state) narrows it.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{MessageAck, ToolEntryView, ToolSourceView};
use serde_json::json;

use super::InvData;
use crate::fmt::{grouped, of};
use crate::widgets::{filter_words, use_toasts, Facet, FacetSet, FilterBar, Ico, Icon, Toasts};

/// The inventory's filter, as the URL holds it: `q` and `show` (a state).
#[derive(Clone, Copy)]
pub(super) struct InvFilter {
    pub query: RwSignal<String>,
    pub facet: RwSignal<String>,
}

impl InvFilter {
    fn narrowed(self) -> bool {
        !self.query.with(|q| q.trim().is_empty()) || !self.facet.with(String::is_empty)
    }

    fn keeps(self, t: &ToolEntryView, words: &[String]) -> bool {
        let f = self.facet.get();
        tool_facet(t, &f) && tool_matches(t, words)
    }
}

pub(super) fn tool_facet(t: &ToolEntryView, facet: &str) -> bool {
    match facet {
        "offered" => t.available,
        "off" => !t.enabled && !t.stale,
        "unavailable" => t.enabled && !t.available && !t.stale,
        "stale" => t.stale,
        _ => true,
    }
}

fn tool_matches(t: &ToolEntryView, words: &[String]) -> bool {
    let hay = format!(
        "{} {} {} {}",
        t.name,
        t.description.as_deref().unwrap_or_default(),
        t.upstream_name.as_deref().unwrap_or_default(),
        t.source_name
    )
    .to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// Which group a tool sits in: its source, or the stale records at the end.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) enum Band {
    Source(String, Option<i64>),
    Stale,
}

impl Band {
    fn holds(&self, t: &ToolEntryView) -> bool {
        match self {
            // A server's tools by its id (its label follows its prefix), a
            // built-in's by its label.
            Band::Source(_, Some(id)) => !t.stale && t.server_id == Some(*id),
            Band::Source(label, None) => {
                !t.stale && &t.source_label == label && t.server_id.is_none()
            }
            Band::Stale => t.stale,
        }
    }

    fn persist_key(&self) -> String {
        match self {
            Band::Source(label, _) => format!("open.mcp.src.{label}"),
            Band::Stale => "open.mcp.src.stale".into(),
        }
    }
}

/// Switch tools one `tool_set` at a time, counting as it goes, then refetch
/// once. There is no bulk op: the loop is the owner's own clicks, made for
/// them, and its progress is on screen while it runs.
fn switch_all(
    names: Vec<String>,
    enable: bool,
    band: String,
    progress: RwSignal<Option<(usize, usize, bool)>>,
    toasts: Toasts,
    inv: InvData,
) {
    let total = names.len();
    if total == 0 || progress.get_untracked().is_some() {
        return;
    }
    progress.set(Some((0, total, enable)));
    spawn_local(async move {
        let mut failed: Vec<String> = Vec::new();
        for (i, name) in names.into_iter().enumerate() {
            if let Err(e) = crate::api::post::<MessageAck, _>(
                "/api/op/tool_set",
                &json!({ "name": name, "enabled": enable }),
            )
            .await
            {
                failed.push(format!("{name}: {e}"));
            }
            let _ = progress.try_set(Some((i + 1, total, enable)));
        }
        let _ = progress.try_set(None);
        inv.load();
        let verb = if enable {
            "switched on"
        } else {
            "switched off"
        };
        if failed.is_empty() {
            toasts.ok(format!("{total} tools {verb} in {band}"));
        } else {
            toasts.err(format!(
                "{} of {total} tools in {band} could not be {verb}: {}",
                failed.len(),
                failed.join("; ")
            ));
        }
    });
}

/// The filter and the groups: every source (`only` none), or one.
#[component]
pub(super) fn Inventory(
    inv: InvData,
    filter: InvFilter,
    /// One source's tools: its label and server id.
    #[prop(optional)]
    only: Option<Band>,
) -> impl IntoView {
    let only = StoredValue::new(only);
    let words = Memo::new(move |_| filter_words(&filter.query.get()));
    let tools = Memo::new(move |_| {
        inv.inv.with(|i| {
            i.as_ref()
                .map(|i| {
                    i.tools
                        .iter()
                        .filter(|t| only.with_value(|o| o.as_ref().is_none_or(|b| b.holds(t))))
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let total = Signal::derive(move || tools.with(Vec::len));
    let shown = Signal::derive(move || {
        let w = words.get();
        tools.with(|ts| ts.iter().filter(|t| filter.keeps(t, &w)).count())
    });
    let facets = Signal::derive(move || {
        tools.with(|ts| {
            [
                ("offered", "Offered"),
                ("off", "Off"),
                ("unavailable", "Unavailable"),
                ("stale", "Stale"),
            ]
            .into_iter()
            .map(|(id, label)| Facet {
                id: id.into(),
                label: label.into(),
                count: ts.iter().filter(|t| tool_facet(t, id)).count(),
            })
            .filter(|f| f.count > 0 || f.id != "stale")
            .collect::<Vec<_>>()
        })
    });
    let bands = Memo::new(move |_| {
        inv.inv.with(|i| {
            let Some(i) = i else {
                return Vec::new();
            };
            if let Some(b) = only.get_value() {
                let src = match &b {
                    Band::Source(label, id) => i
                        .sources
                        .iter()
                        .find(|s| match id {
                            Some(_) => s.server_id == *id,
                            None => &s.label == label && s.server_id.is_none(),
                        })
                        .cloned(),
                    Band::Stale => None,
                };
                return vec![(b, src)];
            }
            let mut v: Vec<(Band, Option<ToolSourceView>)> = i
                .sources
                .iter()
                .map(|s| (Band::Source(s.label.clone(), s.server_id), Some(s.clone())))
                .collect();
            if i.tools.iter().any(|t| t.stale) {
                v.push((Band::Stale, None));
            }
            v
        })
    });
    let fold = only.with_value(Option::is_none);

    view! {
        <div class="inv-filter">
            <FilterBar
                query=filter.query
                placeholder="Filter tools by name or description"
                shown=shown
                total=total
                noun="tools"
                facets=FacetSet {
                    items: facets,
                    active: filter.facet,
                }
            />
        </div>
        {move || {
            inv.error
                .get()
                .map(|e| {
                    view! {
                        <div class="notice err row inv-notice">
                            "Loading the tools failed: "
                            {e}
                            <button class="btn ghost sm" on:click=move |_| inv.load()>
                                "Retry"
                            </button>
                        </div>
                    }
                })
        }}
        <Show when=move || inv.inv.with(Option::is_none) && inv.error.with(Option::is_none)>
            <div class="mdetail-empty">"Loading…"</div>
        </Show>
        <Show when=move || filter.narrowed() && shown.get() == 0 && inv.inv.with(Option::is_some)>
            <p class="pref-desc inv-none">"No tool matches the filter."</p>
        </Show>
        <For each=move || bands.get() key=|(b, s)| format!("{b:?}{s:?}") let:band>
            <ToolBand band=band.0 source=band.1 inv=inv filter=filter words=words fold=fold/>
        </For>
    }
}

/// One source's group: its heading with the counts, Enable and Disable
/// all, and its tools. A source with no tools keeps its group — that is
/// where "the server is disabled" gets said.
#[component]
fn ToolBand(
    band: Band,
    source: Option<ToolSourceView>,
    inv: InvData,
    filter: InvFilter,
    words: Memo<Vec<String>>,
    /// Folds from its heading (the whole inventory); else always open.
    fold: bool,
) -> impl IntoView {
    let toasts = use_toasts();
    let open = if fold {
        crate::prefs::persisted_bool(&band.persist_key(), false)
    } else {
        RwSignal::new(true)
    };
    let band = StoredValue::new(band);
    let all = Memo::new(move |_| {
        inv.inv.with(|i| {
            i.as_ref()
                .map(|i| {
                    band.with_value(|b| {
                        i.tools
                            .iter()
                            .filter(|t| b.holds(t))
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default()
        })
    });
    let shown = Memo::new(move |_| {
        let w = words.get();
        all.with(|ts| {
            ts.iter()
                .filter(|t| filter.keeps(t, &w))
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let narrowed = Signal::derive(move || filter.narrowed());
    // Filtered to nothing, a group of the whole inventory steps aside; a
    // selected source's stays, to say so.
    let visible = move || !fold || !narrowed.get() || shown.with(|s| !s.is_empty());
    let is_open = move || !fold || open.get() || narrowed.get();
    let source_reason = StoredValue::new(source.as_ref().and_then(|s| s.reason.clone()));
    let (label, what) = match &source {
        Some(s) => (
            if fold {
                s.name.clone()
            } else {
                "Tools".to_string()
            },
            format!("{} · {}", s.kind, s.plane),
        ),
        // A server's group before the inventory has its source (just
        // created): the plain heading, nothing said about stale tools.
        None if band.with_value(|b| *b != Band::Stale) => ("Tools".to_string(), String::new()),
        None => (
            "Stale".to_string(),
            "switched off here, but no longer offered by anything".to_string(),
        ),
    };
    let band_name = StoredValue::new(source.as_ref().map_or_else(
        || {
            band.with_value(|b| match b {
                Band::Source(label, _) => label.clone(),
                Band::Stale => "Stale".to_string(),
            })
        },
        |s| s.name.clone(),
    ));
    let count = move || shown.with(|s| of(s.len(), all.with(Vec::len)));
    let meta = {
        let reason = source_reason.get_value();
        move || {
            let (on, off, unavailable) = all.with(|ts| {
                (
                    ts.iter().filter(|t| t.available).count(),
                    ts.iter().filter(|t| !t.enabled).count(),
                    ts.iter().filter(|t| t.enabled && !t.available).count(),
                )
            });
            let mut parts = vec![format!("{on} on"), format!("{off} off")];
            if unavailable > 0 {
                parts.push(format!("{unavailable} unavailable"));
            }
            if !what.is_empty() {
                parts.push(what.clone());
            }
            if let Some(r) = &reason {
                parts.push(r.clone());
            }
            parts.join(" · ")
        }
    };
    let progress = RwSignal::new(None::<(usize, usize, bool)>);
    // Enable/Disable all act on what the group shows: all of it, or what a
    // filter left of it — and say which.
    let targets = move |enable: bool| {
        shown.with(|ts| {
            ts.iter()
                .filter(|t| t.enabled != enable)
                .map(|t| t.name.clone())
                .collect::<Vec<_>>()
        })
    };
    let actions = move || {
        // Folded, a group is its heading and its counts.
        if band.with_value(|b| *b == Band::Stale) || !is_open() {
            return ().into_any();
        }
        if let Some((done, total, enable)) = progress.get() {
            let verb = if enable { "Enabling" } else { "Disabling" };
            return view! { <span class="bulk-progress">{format!("{verb} {done}/{total}…")}</span> }
                .into_any();
        }
        let whole = shown.with(Vec::len) == all.with(Vec::len);
        let button = move |enable: bool| {
            let n = targets(enable).len();
            let (verb, state) = if enable {
                ("Enable", "off")
            } else {
                ("Disable", "on")
            };
            let label = if whole {
                format!("{verb} all")
            } else {
                format!("{verb} {n} shown")
            };
            let title = if n == 0 {
                format!("Nothing here is {state}")
            } else {
                format!(
                    "Switch {} the {n} tool{} that {} {state} — applies now",
                    if enable { "on" } else { "off" },
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "is" } else { "are" },
                )
            };
            view! {
                <button
                    type="button"
                    class="btn flat sm"
                    disabled=n == 0
                    title=title
                    on:click=move |_| {
                        switch_all(targets(enable), enable, band_name.get_value(), progress, toasts, inv)
                    }
                >
                    {label}
                </button>
            }
        };
        view! { {button(true)} {button(false)} }.into_any()
    };
    let label = StoredValue::new(label);
    let head = move || {
        if fold {
            view! {
                <button
                    type="button"
                    class="pref-fold"
                    class:open=is_open
                    aria-expanded=move || is_open().to_string()
                    disabled=move || narrowed.get()
                    on:click=move |_| open.update(|v| *v = !*v)
                >
                    <Icon ico=Ico::Right class="pref-fold-chev"/>
                    <h2>{label.get_value()}</h2>
                    <span class="pref-fold-sub">{count}</span>
                </button>
            }
            .into_any()
        } else {
            view! {
                <h2>{label.get_value()}</h2>
                <span class="pref-fold-sub">{count}</span>
            }
            .into_any()
        }
    };

    view! {
        <Show when=visible>
            <section class="pref-group tool-band" data-band=band.with_value(|b| format!("{b:?}"))>
                <div class="pref-head">
                    {head()}
                    <span class="pref-suffix">{actions}</span>
                </div>
                <p class="pref-desc tool-band-meta" title=meta.clone()>{meta.clone()}</p>
                <Show when=is_open>
                    <div class="boxed">
                        <For
                            each=move || shown.get()
                            key=|t| {
                                (
                                    t.name.clone(),
                                    t.enabled,
                                    t.available,
                                    t.stale,
                                    t.reason.clone(),
                                    t.description.clone(),
                                )
                            }
                            let:t
                        >
                            <ToolRow t=t source_reason=source_reason.get_value() inv=inv/>
                        </For>
                        <Show when=move || all.with(Vec::is_empty)>
                            <div class="prow notice-row">"No tools right now"</div>
                        </Show>
                        <Show when=move || all.with(|a| !a.is_empty()) && shown.with(Vec::is_empty)>
                            <div class="prow notice-row">"No tool here matches the filter"</div>
                        </Show>
                    </div>
                </Show>
            </section>
        </Show>
    }
}

/// One tool: its name (the source's prefix dimmed, the same on every row of
/// its group), its description, its state, a reason only when it is not the
/// obvious one — a tool the owner switched off says "off", and its group
/// already says why its source is down — and the owner's switch.
#[component]
fn ToolRow(t: ToolEntryView, source_reason: Option<String>, inv: InvData) -> impl IntoView {
    let toasts = use_toasts();
    let name = StoredValue::new(t.name.clone());
    let (enabled, available, stale) = (t.enabled, t.available, t.stale);
    let busy = RwSignal::new(false);
    let toggle = move |ev: leptos::ev::MouseEvent| {
        ev.stop_propagation();
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let res = crate::api::post::<MessageAck, _>(
                "/api/op/tool_set",
                &json!({ "name": name.get_value(), "enabled": !enabled }),
            )
            .await;
            let _ = busy.try_set(false);
            if let Err(e) = res {
                toasts.err(e.to_string());
            }
            inv.load();
        });
    };
    let (chip, label) = if stale {
        ("chip warn", "Stale")
    } else if available {
        ("chip ok", "Offered")
    } else if !enabled {
        ("chip off", "Off")
    } else {
        ("chip off", "Unavailable")
    };
    let why = t
        .reason
        .clone()
        .filter(|_| enabled || stale)
        .filter(|r| Some(r) != source_reason.as_ref());
    let prefix = format!("{}__", t.source_label);
    let (pfx, base) = match t.name.strip_prefix(&prefix) {
        Some(rest) => (prefix.clone(), rest.to_string()),
        None => (String::new(), t.name.clone()),
    };
    let desc = t.description.clone().unwrap_or_default();
    let tip = if enabled {
        "Offered — switch it off here (applies now)"
    } else {
        "Switched off here — switch it on to offer it again (applies now)"
    };
    view! {
        <div class="prow tool-row" class:muted=!enabled>
            <div class="prow-text">
                <span class="prow-title tool-name" title=t.name.clone()>
                    <span class="pfx">{pfx}</span>
                    {base}
                </span>
                {(!desc.is_empty()).then(|| view! { <span class="prow-sub tool-desc" title=desc.clone()>{desc.clone()}</span> })}
                {why.map(|w| view! { <span class="prow-sub tool-why">{w}</span> })}
            </div>
            <div class="prow-end">
                <span class=chip title=t.reason.clone().unwrap_or_default()>
                    <span class="dot"></span>
                    {label}
                </span>
                <button
                    type="button"
                    class="switch"
                    class:on=enabled
                    role="switch"
                    aria-checked=enabled.to_string()
                    aria-label=t.name.clone()
                    title=tip
                    disabled=move || busy.get()
                    on:click=toggle
                ></button>
            </div>
        </div>
    }
}

/// The inventory's counts, for the overview's heading.
pub(super) fn summary(inv: InvData) -> String {
    inv.inv.with(|i| {
        i.as_ref().map_or_else(String::new, |i| {
            let n = |f: &str| i.tools.iter().filter(|t| tool_facet(t, f)).count();
            format!(
                "{} tools · {} offered · {} off",
                grouped(i.tools.len() as u64),
                grouped(n("offered") as u64),
                grouped(n("off") as u64)
            )
        })
    })
}
