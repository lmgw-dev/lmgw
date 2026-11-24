//! The detail pane: nothing selected (the whole tool inventory), a server
//! ([`super::editor`]), a new one, a built-in toolset (its tools), or a
//! selection the gateway does not have.

use leptos::prelude::*;

use super::tools::{summary, Band, Inventory};
use super::{kept_query, Page, Sel};
use crate::widgets::nav_split::{track_scroll, DetailHead};
use crate::widgets::{ActionRow, Ico, Icon, PrefGroup};

#[component]
pub(super) fn DetailPane(
    page: Page,
    sel: Memo<Option<Sel>>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    // Whether the selection is (still) there; not known yet counts as there.
    let known = Memo::new(move |_| match sel.get() {
        Some(Sel::Server(id)) => page
            .servers
            .rows
            .with(|r| r.as_ref().map(|r| r.iter().any(|s| s.id == id))),
        Some(Sel::Builtin(label)) => page.inv.inv.with(|i| {
            i.as_ref().map(|i| {
                i.sources
                    .iter()
                    .any(|s| s.kind == "builtin" && s.label == label)
            })
        }),
        _ => Some(true),
    });
    let present = Memo::new(move |_| known.get() != Some(false));
    // The shown server went away (its Delete): back to the list.
    let location = leptos_router::hooks::use_location();
    Effect::new(move |prev: Option<(Option<Sel>, Option<bool>)>| {
        let now = (sel.get(), known.get());
        if let Some((was, Some(true))) = prev {
            if was == now.0 && now.1 == Some(false) {
                let q = kept_query(&location.search.get_untracked());
                page.nav.go_unasked(&format!("/mcp-servers{q}"), true);
            }
        }
        now
    });
    move || {
        let s = sel.get();
        if !present.get() {
            return view! { <Gone narrow=narrow back=back/> }.into_any();
        }
        match s {
            None => view! { <Overview page=page narrow=narrow back=back/> }.into_any(),
            Some(Sel::New) => {
                view! { <super::editor::ServerDetail page=page id=None narrow=narrow back=back/> }
                    .into_any()
            }
            Some(Sel::Server(id)) => view! {
                <super::editor::ServerDetail page=page id=Some(id) narrow=narrow back=back/>
            }
            .into_any(),
            Some(Sel::Builtin(label)) => {
                view! { <BuiltinDetail page=page label=label narrow=narrow back=back/> }.into_any()
            }
        }
    }
}

/// Nothing selected: everything the gateway serves northbound. The list is
/// the config plane (which servers exist); this is the surface they add up
/// to, with the built-in toolsets, which were served to every connected
/// agent with nothing in the dashboard that even named them.
#[component]
fn Overview(page: Page, narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    view! {
        <DetailHead narrow=narrow back=back title="MCP servers" scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="detail-flow mcp-flow mcp-overview">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Wrench/>
                    </div>
                    <div class="hero-text">
                        <h1>"Tool inventory"</h1>
                        <div class="sub">
                            "Every tool lmgw serves on /mcp, by where it comes from. A tool switched off is neither listed nor callable; a switch applies at once."
                        </div>
                        <div class="chips">
                            <span class="chip">{move || summary(page.inv)}</span>
                        </div>
                    </div>
                </div>
                <PrefGroup>
                    <ActionRow
                        title="Add server"
                        sub="A stdio command (in a container or bare), or an HTTP or SSE endpoint"
                        chevron=true
                        on_click=Callback::new(move |()| {
                            page.nav.go(&Sel::New.path(), false);
                        })
                    />
                </PrefGroup>
                <Inventory inv=page.inv filter=page.filter/>
            </div>
        </div>
    }
}

/// A built-in toolset: what it is and its tools.
#[component]
fn BuiltinDetail(
    page: Page,
    label: String,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    let lbl = StoredValue::new(label.clone());
    let source = Memo::new(move |_| {
        page.inv.inv.with(|i| {
            i.as_ref().and_then(|i| {
                lbl.with_value(|l| {
                    i.sources
                        .iter()
                        .find(|s| s.kind == "builtin" && &s.label == l)
                        .cloned()
                })
            })
        })
    });
    let name = Signal::derive(move || {
        source
            .get()
            .map(|s| s.name)
            .unwrap_or_else(|| lbl.get_value())
    });
    view! {
        <DetailHead narrow=narrow back=back title=name scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="detail-flow mcp-flow">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=if label == "docs" { Ico::Book } else { Ico::Sliders }/>
                    </div>
                    <div class="hero-text">
                        <h1>{move || name.get()}</h1>
                        <div class="sub">
                            <code>{format!("{label}__*")}</code>
                            " · built in"
                        </div>
                        <div class="chips">
                            {move || {
                                source
                                    .get()
                                    .map(|s| {
                                        let (cls, word) = if s.available {
                                            ("chip ok", "Offered")
                                        } else {
                                            ("chip off", "Off")
                                        };
                                        view! {
                                            <span class=cls title=s.reason.clone().unwrap_or_default()>
                                                <span class="dot"></span>
                                                {word}
                                            </span>
                                            <span class="chip">{format!("on {}", s.plane)}</span>
                                            <span class="chip">
                                                {format!(
                                                    "{}/{} tools on",
                                                    (s.tool_count - s.disabled_count).max(0),
                                                    s.tool_count,
                                                )}
                                            </span>
                                        }
                                    })
                            }}
                        </div>
                    </div>
                </div>
                {move || {
                    source
                        .get()
                        .and_then(|s| s.reason)
                        .map(|r| {
                            view! {
                                <PrefGroup>
                                    <div class="prow notice-row warn">{r}</div>
                                </PrefGroup>
                            }
                        })
                }}
                <Inventory inv=page.inv filter=page.filter only=Band::Source(label.clone(), None)/>
            </div>
        </div>
    }
}

/// A selection that names a server the gateway does not have.
#[component]
fn Gone(narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    view! {
        <DetailHead narrow=narrow back=back title="Not here" scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll">
            <div class="detail-flow">
                <div class="mdetail-empty">
                    <p>"This server is not in the gateway's config: it was deleted, or the link names one that never was."</p>
                    <button type="button" class="btn" on:click=move |_| back.run(())>
                        "Back to the list"
                    </button>
                </div>
            </div>
        </div>
    }
}
