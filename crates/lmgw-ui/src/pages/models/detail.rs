//! The detail pane (modern-sample.html): a header bar with the selected
//! row's actions (Start or Stop, Test, and a menu with the rest), and under
//! it the hero — what the row is, where its container is — and its settings
//! as preference groups, which flow into two columns in a pane over 1000 px
//! (`.detail-flow`). A Delete row closes the page.
//!
//! The local chat class is the whole editor ([`local`]). The other classes
//! keep their editor forms for now, in the pane ([`embedded`]), with the same
//! header, hero and Delete around them.

use leptos::prelude::*;

use super::route::Sel;
use super::{rows_of, run_test, runtime_of, Class, PageCtx, Row};
use crate::ops_state::model_key;
use crate::widgets::{ActionRow, Ico, Icon, InfoRow, MenuItem, PrefGroup};

/// Another class's editor form in the pane.
mod embedded;
/// The hero: icon, name, file, and the chips of its state.
mod hero;
/// A local chat model: the whole editor.
mod local;

use hero::Hero;

pub(super) use crate::widgets::nav_split::track_scroll;

#[component]
pub(super) fn DetailPane(
    page: PageCtx,
    sel: Memo<Option<Sel>>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    let data = page.data;
    let rows = Memo::new(move |_| data.full.with(|f| f.as_ref().map(rows_of)));
    // Whether the selected row is (still) there: a deleted row, or a link
    // to one that never was, says so instead of showing an empty form. Not
    // known yet counts as there, so the page does not flash "gone".
    let known = Memo::new(move |_| match sel.get() {
        Some(Sel::Row(c, id)) => rows.with(|rs| {
            rs.as_ref()
                .map(|rs| rs.iter().any(|r| r.class == c && r.id == id))
        }),
        Some(Sel::Catalog(id)) => data
            .full
            .with(|f| f.as_ref().map(|f| f.passthrough.iter().any(|p| p.id == id))),
        _ => Some(true),
    });
    let present = Memo::new(move |_| known.get() != Some(false));
    // The shown row went away (its Delete): back to the list, without
    // asking about its unsaved edits, which have no row left to go to. A
    // link to a row that never was says so instead (`Gone`).
    let location = leptos_router::hooks::use_location();
    Effect::new(move |prev: Option<(Option<Sel>, Option<bool>)>| {
        let now = (sel.get(), known.get());
        if let Some((was_sel, Some(true))) = prev {
            if was_sel == now.0 && now.1 == Some(false) {
                let q = super::route::list_query(&location.search.get_untracked());
                page.go_unasked(&format!("/models{q}"), true);
            }
        }
        now
    });
    move || {
        let s = sel.get();
        if !present.get() {
            return view! { <Gone narrow=narrow back=back/> }.into_any();
        }
        let row = Signal::derive(move || {
            let Some(Sel::Row(c, id)) = s else {
                return None;
            };
            rows.with(|rs| {
                rs.as_ref()
                    .and_then(|rs| rs.iter().find(|r| r.class == c && r.id == id).cloned())
            })
        });
        match s {
            None => view! { <StatusPage page=page narrow=narrow back=back rows=rows/> }.into_any(),
            Some(Sel::NewChat) => {
                view! { <local::LocalDetail page=page id=None row=row narrow=narrow back=back/> }
                    .into_any()
            }
            Some(Sel::Row(Class::Chat, id)) => {
                view! { <local::LocalDetail page=page id=Some(id) row=row narrow=narrow back=back/> }
                    .into_any()
            }
            Some(Sel::Row(class, id)) => view! {
                <RowDetail page=page class=class id=id row=row narrow=narrow back=back/>
            }
            .into_any(),
            Some(Sel::Catalog(id)) => {
                view! { <CatalogDetail page=page id=id narrow=narrow back=back/> }.into_any()
            }
        }
    }
}

pub(super) use crate::widgets::nav_split::DetailHead;

/// Start, or Stop while its container is up, for a class with a container.
/// A disabled row is not served: Start waits for Enable (a container would
/// hold VRAM for nothing); Stop stays, which is how one left running goes.
#[component]
pub(super) fn RunButton(
    page: PageCtx,
    class: Class,
    #[prop(into)] model_id: Signal<String>,
    #[prop(into)] enabled: Signal<bool>,
) -> impl IntoView {
    let live = crate::live::use_live();
    let Some(rc) = class.runtime() else {
        return ().into_any();
    };
    let up = move || {
        let mid = model_id.get();
        live.runtime.with(|rows| {
            rows.as_deref()
                .is_some_and(|rows| runtime_of(rows, class, &mid).is_some())
        })
    };
    let busy = move || page.ops.busy(&model_key(rc, &model_id.get()));
    view! {
        {move || {
            if up() {
                view! {
                    <button
                        type="button"
                        class="btn flat"
                        disabled=busy
                        title="Stop its container"
                        on:click=move |_| page.container(rc, model_id.get_untracked(), "stop")
                    >
                        <Icon ico=Ico::Stop/>
                        "Stop"
                    </button>
                }
                .into_any()
            } else {
                view! {
                    <button
                        type="button"
                        class="btn flat"
                        disabled=move || busy() || !enabled.get() || model_id.with(String::is_empty)
                        title=move || {
                            if enabled.get() {
                                "Start its container now; may evict other models under the VRAM ledger"
                            } else {
                                "Enable it first: a disabled model is not served"
                            }
                        }
                        on:click=move |_| page.container(rc, model_id.get_untracked(), "start")
                    >
                        <Icon ico=Ico::Play/>
                        {move || if busy() { "Starting…" } else { "Start" }}
                    </button>
                }
                .into_any()
            }
        }}
    }
    .into_any()
}

/// The container's items of a row's menu: Restart and Apply while it is up,
/// Logs always.
pub(super) fn container_items(
    page: PageCtx,
    live: crate::live::LiveBus,
    class: Class,
    model_id: String,
    enabled: bool,
) -> Vec<MenuItem> {
    let Some(rc) = class.runtime() else {
        return Vec::new();
    };
    let mid = StoredValue::new(model_id);
    let up = live.runtime.with(|rows| {
        rows.as_deref()
            .is_some_and(|rows| mid.with_value(|m| runtime_of(rows, class, m).is_some()))
    });
    let busy = mid.with_value(|m| page.ops.busy(&model_key(rc, m)));
    let mut v = Vec::new();
    if up {
        v.push(
            MenuItem::new("Restart", move || {
                page.container(rc, mid.get_value(), "restart")
            })
            .disabled(busy || !enabled)
            .title(if enabled {
                "Stop and start its container again, with the saved configuration"
            } else {
                "Enable it first: a disabled model is not served"
            }),
        );
        v.push(
            MenuItem::new("Apply to container", move || {
                page.container(rc, mid.get_value(), "apply")
            })
            .disabled(busy)
            .title("Stop and restart this model's container now, with the current configuration"),
        );
    }
    v.push(
        MenuItem::new("Logs", move || page.logs.set(Some((rc, mid.get_value()))))
            .title("podman logs for this model's container"),
    );
    v
}

/// "Copy path": what the list's file cell used to copy (a GGUF's full
/// path, an audio model's directory, an alias's upstream model id).
pub(super) fn copy_item(page: PageCtx, class: Class, text: String) -> Option<MenuItem> {
    if text.trim().is_empty() {
        return None;
    }
    let label = match class {
        Class::Alias => "Copy the upstream model id",
        Class::Candidate => "Copy the primary",
        _ => "Copy path",
    };
    Some(MenuItem::new(label, move || {
        crate::widgets::copy_secret(&text, page.toasts, format!("Copied {text}"))
    }))
}

/// A configured row of a class whose editor is its form, in the pane.
#[component]
fn RowDetail(
    page: PageCtx,
    class: Class,
    id: i64,
    row: Signal<Option<Row>>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    let live = crate::live::use_live();
    let testing = RwSignal::new(false);
    let scrolled = RwSignal::new(false);
    let title = Signal::derive(move || row.get().map(|r| r.name).unwrap_or_default());
    // The file's first line (an image pipeline: its entry point and how
    // many files hang off it); the rest is in the settings below.
    let sub = Signal::derive(move || {
        row.get()
            .map(|r| match r.class {
                Class::Image => r.file,
                _ => r.file_title.lines().next().unwrap_or_default().to_string(),
            })
            .unwrap_or_default()
    });
    let mid = Signal::derive(move || row.get().map(|r| r.model_id).unwrap_or_default());
    let enabled = Signal::derive(move || row.get().is_some_and(|r| r.enabled));
    let more = Signal::derive(move || {
        let Some(r) = row.get() else {
            return Vec::new();
        };
        let mut v = container_items(page, live, class, r.model_id.clone(), r.enabled);
        v.extend(copy_item(page, class, r.copy.clone()));
        let toggle = if r.enabled { "disable" } else { "enable" };
        v.push(MenuItem::new(
            if r.enabled { "Disable" } else { "Enable" },
            move || {
                page.op(
                    class.op(),
                    serde_json::json!({ "action": toggle, "id": id }),
                )
            },
        ));
        v.push(local::delete_item(page, id, row).title(match class {
            Class::Alias | Class::Candidate => "Remove the alias from the gateway config",
            _ => "Remove it from the gateway config and stop its container; the files on disk stay",
        }));
        v
    });
    let probe = move || match class {
        Class::Aux => row.with(|r| r.as_ref().map(|r| super::aux_row::probe_of(&r.kind))),
        other => other.probe(),
    };
    view! {
        <DetailHead
            narrow=narrow
            back=back
            title=title
            scrolled=scrolled
            actions=move || {
                view! {
                    <RunButton page=page class=class model_id=mid enabled=enabled/>
                    {move || {
                        probe()
                            .map(|probe| {
                                view! {
                                    <button
                                        type="button"
                                        class="btn flat"
                                        disabled=move || testing.get() || !enabled.get()
                                        title=if enabled.get_untracked() {
                                            format!(
                                                "Load test: starts this model's container if it is not running (which may evict others under the VRAM ledger), then {probe}"
                                            )
                                        } else {
                                            "Enable it first: a disabled model is not served, so there is nothing to test".to_string()
                                        }
                                        on:click=move |_| run_test(page.toasts, class, mid.get_untracked(), testing)
                                    >
                                        <Icon ico=Ico::Check/>
                                        {move || if testing.get() { "Testing…" } else { "Test" }}
                                    </button>
                                }
                            })
                    }}
                }
            }
            more=more
        />
        <div
            class="detail-scroll mdetail-scroll"
            tabindex="-1"
            on:scroll=move |ev| track_scroll(&ev, scrolled)
        >
            <div class="detail-flow">
                <Hero class=class name=title sub=sub row=row/>
                <embedded::EmbeddedForm page=page class=class id=id/>
                <PrefGroup>
                    <ActionRow
                        title=match class {
                            Class::Alias | Class::Candidate => "Delete alias…",
                            _ => "Delete model…",
                        }
                        sub=match class {
                            Class::Alias => "Removes the alias; the upstream model it points at is not touched",
                            Class::Candidate => "Removes the alias; the local models it points at are not touched",
                            _ => "Removes it from the gateway config and stops its container; the files on disk stay",
                        }
                        danger=true
                        on_click=Callback::new(move |()| {
                            if let Some(r) = row.get_untracked() {
                                page.deleting.set(Some((class, id, r.name)));
                            }
                        })
                    />
                </PrefGroup>
            </div>
        </div>
    }
}

/// A passthrough upstream: its request prefix, its size, the way to its
/// catalog.
#[component]
fn CatalogDetail(page: PageCtx, id: i64, narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let catalog = crate::catalog::use_model_catalog();
    let line = Memo::new(move |_| {
        page.data.full.with(|f| {
            f.as_ref()
                .and_then(|f| f.passthrough.iter().find(|p| p.id == id).cloned())
        })
    });
    let name = Signal::derive(move || line.get().map(|l| l.upstream).unwrap_or_default());
    let listed = Signal::derive(move || {
        let n = name.get();
        catalog
            .entries
            .with(|es| es.iter().filter(|e| !e.local && e.source == n).count())
    });
    let hidden = Signal::derive(move || line.get().map_or(0, |l| l.hidden.len()));
    let prefix = Signal::derive(move || {
        line.get()
            .map(|l| l.prefix.trim_matches('/').to_string())
            .unwrap_or_default()
    });
    let scrolled = RwSignal::new(false);
    let open = move || {
        let href = format!(
            "/models/catalog?upstream={}",
            String::from(js_sys::encode_uri_component(&name.get_untracked()))
        );
        page.go(&href, false);
    };
    view! {
        <DetailHead narrow=narrow back=back title=name scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="detail-flow">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Models/>
                    </div>
                    <div style="min-width:0">
                        <h1>{move || name.get()}</h1>
                        <div class="sub">"Every model of an expose-all upstream, resolved live"</div>
                    </div>
                </div>
                <PrefGroup title="Catalog">
                    <InfoRow title="Requested as" value=move || format!("{}/<id>", prefix.get()) mono=true/>
                    <InfoRow
                        title="Models"
                        value=move || {
                            let n = listed.get() + hidden.get();
                            if n == 0 && catalog.loading.get() {
                                "counting…".to_string()
                            } else if n == 0 {
                                "none listed right now — the catalog view says why".to_string()
                            } else {
                                crate::fmt::grouped(n as u64)
                            }
                        }
                    />
                    <InfoRow title="Hidden" value=move || crate::fmt::grouped(hidden.get() as u64)/>
                    <ActionRow title="Open catalog" chevron=true on_click=Callback::new(move |()| open())/>
                    <ActionRow
                        title="Upstreams"
                        sub="Where the upstream and its prefix are set"
                        chevron=true
                        on_click=Callback::new(move |()| {
                            page.go("/upstreams", false);
                        })
                    />
                </PrefGroup>
            </div>
        </div>
    }
}

/// Nothing selected: what the list holds, and the ways to add.
#[component]
fn StatusPage(
    page: PageCtx,
    narrow: Memo<bool>,
    back: Callback<()>,
    rows: Memo<Option<Vec<Row>>>,
) -> impl IntoView {
    let live = crate::live::use_live();
    let location = leptos_router::hooks::use_location();
    let scrolled = RwSignal::new(false);
    let summary = move || {
        let rt = live.runtime.get().unwrap_or_default();
        rows.with(|rs| {
            let Some(rs) = rs else {
                return Vec::new();
            };
            Class::ALL
                .iter()
                .filter_map(|c| {
                    let mine: Vec<&Row> = rs.iter().filter(|r| r.class == *c).collect();
                    if mine.is_empty() {
                        return None;
                    }
                    let up = mine.iter().filter(|r| super::is_running(&rt, r)).count();
                    let off = mine.iter().filter(|r| !r.enabled).count();
                    let mut parts = Vec::new();
                    if up > 0 {
                        parts.push(format!("{up} running"));
                    }
                    if off > 0 {
                        parts.push(format!("{off} disabled"));
                    }
                    Some((*c, c.label(), mine.len(), parts.join(" · ")))
                })
                .collect::<Vec<_>>()
        })
    };
    view! {
        <DetailHead narrow=narrow back=back title="Models" scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="detail-flow mstatus">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Models/>
                    </div>
                    <div style="min-width:0">
                        <h1>"Models"</h1>
                        <div class="sub">"Everything the gateway can serve. Pick one in the list, or add one."</div>
                    </div>
                </div>
                <PrefGroup title="In the list">
                    {move || {
                        summary()
                            .into_iter()
                            .map(|(class, label, n, note)| {
                                let search = location.search.get();
                                let rest = crate::widgets::list_pane::query_without(&search, &["show"]);
                                let sep = if rest.is_empty() { '?' } else { '&' };
                                let href = format!("/models{rest}{sep}show={}", class.id());
                                view! {
                                    <a class="prow action" href=href>
                                        <div class="prow-text">
                                            <span class="prow-title">{label}</span>
                                            {(!note.is_empty()).then(|| view! { <span class="prow-sub">{note}</span> })}
                                        </div>
                                        <span class="prow-value">{n}</span>
                                        <Icon ico=Ico::Right class="prow-chevron"/>
                                    </a>
                                }
                            })
                            .collect_view()
                    }}
                </PrefGroup>
                <PrefGroup title="Add">
                    // A link, as the way to another page is.
                    <a class="prow action" href="/models/find">
                        <div class="prow-text">
                            <span class="prow-title">"Find a model"</span>
                            <span class="prow-sub">
                                "By the name someone told you: lmgw picks the file that fits your graphics card"
                            </span>
                        </div>
                        <Icon ico=Ico::Right class="prow-chevron"/>
                    </a>
                    <ActionRow
                        title="Add from Hugging Face"
                        sub="Search a repository, pick a file, download it"
                        chevron=true
                        on_click=Callback::new(move |()| page.wizard_open.set(true))
                    />
                </PrefGroup>
            </div>
        </div>
    }
}

/// A selection that names a row the gateway does not have (deleted, or a
/// link to one that never was).
#[component]
fn Gone(narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    view! {
        <DetailHead narrow=narrow back=back title="Not here" scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll">
            <div class="detail-flow">
                <div class="mdetail-empty">
                    <p>"This model is not in the gateway's config: it was deleted, or the link names one that never was."</p>
                    <button type="button" class="btn" on:click=move |_| back.run(())>
                        "Back to the list"
                    </button>
                </div>
            </div>
        </div>
    }
}
