//! The operations list pane (api-docs design §6.2, §6.4): the header bar
//! with how many operations the filter shows and the filter popover (the
//! group and the method), the search, and the operations under their
//! group's and tag's sticky headings, each a row with its method and name
//! over its path. `page.rs` hands this the filtered list in rail order
//! (`ApiDoc::rail_order` plus `ApiDoc::matches`); this module is
//! presentation only.

use leptos::html;
use leptos::prelude::*;

use super::doc::Operation;
use crate::widgets::list_pane::{GroupHead, ListBox, ListHead, ListNav, ListSearch};
use crate::widgets::{Ico, Icon, Popover};

#[derive(Clone, PartialEq)]
// Short-lived render rows; boxing `Op` would ripple through every match arm for no gain.
#[allow(clippy::large_enum_variant)]
enum Line {
    /// The tag group's eyebrow (§6.2: group → tag head → items).
    Group(String),
    Head {
        label: String,
        count: usize,
    },
    Op(Operation),
}

fn group(ops: &[Operation]) -> Vec<Line> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut last_group: Option<&str> = None;
    while i < ops.len() {
        if last_group != Some(ops[i].group.as_str()) {
            last_group = Some(ops[i].group.as_str());
            if !ops[i].group.is_empty() {
                out.push(Line::Group(ops[i].group.clone()));
            }
        }
        let label = ops[i].tag_name.clone();
        let j = ops[i..]
            .iter()
            .position(|o| o.tag_name != label)
            .map_or(ops.len(), |k| i + k);
        out.push(Line::Head {
            label,
            count: j - i,
        });
        out.extend(ops[i..j].iter().cloned().map(Line::Op));
        i = j;
    }
    out
}

/// The label a row shows: the op name for `/api/op/{name}`, the path
/// otherwise (§6.2).
pub fn op_label(op: &Operation) -> String {
    op.op_name.clone().unwrap_or_else(|| op.path.clone())
}

fn line_key(l: &Line) -> String {
    match l {
        Line::Group(label) => format!("g:{label}"),
        Line::Head { label, count } => format!("h:{label}:{count}"),
        Line::Op(op) => format!("o:{}", op.operation_id),
    }
}

/// The filters the list's popover holds: one group, one method.
#[derive(Clone, Copy)]
pub struct Facets {
    pub group: RwSignal<String>,
    pub method: RwSignal<String>,
    pub groups: Memo<Vec<(String, usize)>>,
    pub methods: Memo<Vec<(String, usize)>>,
}

#[component]
pub fn OpsPane(
    nav: ListNav,
    narrow: Memo<bool>,
    query: RwSignal<String>,
    #[prop(into)] operations: Signal<Vec<Operation>>,
    #[prop(into)] total: Signal<usize>,
    #[prop(into)] loaded: Signal<bool>,
    facets: Facets,
) -> impl IntoView {
    let lines = Memo::new(move |_| group(&operations.get()));
    let open = RwSignal::new(false);
    let btn: NodeRef<html::Button> = NodeRef::new();
    let filtered =
        move || !facets.group.with(String::is_empty) || !facets.method.with(String::is_empty);
    let sub = Signal::derive(move || {
        if !loaded.get() {
            return "Loading…".to_string();
        }
        let (n, all) = (operations.with(Vec::len), total.get());
        if n == all {
            crate::fmt::count_of(all, "operations")
        } else {
            format!("{n} of {}", crate::fmt::count_of(all, "operations"))
        }
    });
    let actions = ViewFn::from(move || {
        view! {
            <button
                type="button"
                class="icon-btn api-filter-btn"
                class:on=filtered
                node_ref=btn
                title="Filter by group and method"
                aria-label="Filters"
                aria-expanded=move || open.get().to_string()
                on:click=move |_| open.update(|o| *o = !*o)
            >
                <Icon ico=Ico::Sliders/>
            </button>
            <Popover open=open anchor=btn class="api-facets-pop" min_width=280>
                <FacetPicker facets=facets/>
            </Popover>
        }
    });
    let render = move |l: Line| -> AnyView {
        match l {
            Line::Group(label) => {
                view! { <div class="api-group" role="presentation">{label}</div> }.into_any()
            }
            Line::Head { label, count } => view! { <GroupHead label=label n=count/> }.into_any(),
            Line::Op(op) => {
                let id = op.operation_id.clone();
                let (lit, lit2, click) = (id.clone(), id.clone(), id.clone());
                let label = op_label(&op);
                let method = op.method.clone();
                let mut tip = format!("{} {}", op.method, op.path);
                if !op.summary.is_empty() {
                    tip.push('\n');
                    tip.push_str(&op.summary);
                }
                let meta = if op.op_name.is_some() {
                    op.path.clone()
                } else if op.summary.is_empty() {
                    op.tag_name.clone()
                } else {
                    op.summary.clone()
                };
                // The kit's list row, with the method's badge where its icon
                // would be.
                view! {
                    <div
                        class="lrow has-ico api-row"
                        data-key=format!("/api-reference/{id}")
                        data-op=id.clone()
                        title=tip
                        role="option"
                        class:sel=move || nav.lit(&lit)
                        aria-selected=move || nav.lit(&lit2).to_string()
                        on:click=move |_| nav.click(click.clone())
                    >
                        <span class=format!("method lrow-ico m-{}", method.to_ascii_lowercase())>
                            {method.clone()}
                        </span>
                        <span class="lrow-name mono">{label}</span>
                        <span class="lrow-meta">{meta}</span>
                    </div>
                }
                .into_any()
            }
        }
    };
    let chip = move |sig: RwSignal<String>| {
        move || {
            let v = sig.get();
            (!v.is_empty()).then(|| {
                view! {
                    <span class="chip filter-chip">
                        {v}
                        <button
                            type="button"
                            class="chip-x"
                            title="Remove this filter"
                            aria-label="Remove this filter"
                            on:click=move |_| sig.set(String::new())
                        >
                            <Icon ico=Ico::Close/>
                        </button>
                    </span>
                }
            })
        }
    };
    view! {
        <ListHead scrolled=nav.scrolled narrow=narrow title="API reference" sub=sub actions=actions/>
        <ListSearch
            nav=nav
            query=query
            placeholder="Search operations"
            title="Path, summary, op name or tag"
        />
        <Show when=filtered>
            <div class="mlist-chips">{chip(facets.group)} {chip(facets.method)}</div>
        </Show>
        <ListBox nav=nav label="API operations">
            <Show when=move || loaded.get() && operations.with(Vec::is_empty)>
                <div class="mlist-empty">"No operation matches."</div>
            </Show>
            <For each=move || lines.get() key=line_key let:l>
                {render(l)}
            </For>
        </ListBox>
    }
}

/// The popover's two pickers.
#[component]
fn FacetPicker(facets: Facets) -> impl IntoView {
    let picker = move |label: &'static str,
                       any: &'static str,
                       items: Memo<Vec<(String, usize)>>,
                       active: RwSignal<String>| {
        view! {
            <div class="api-facet">
                <div class="api-facet-label">{label}</div>
                <div class="facets" role="group" aria-label=label>
                    <button
                        type="button"
                        class="facet"
                        class:on=move || active.with(String::is_empty)
                        on:click=move |_| active.set(String::new())
                    >
                        {any}
                    </button>
                    <For each=move || items.get() key=|(m, _)| m.clone() let:row>
                        {
                            let (m, n) = row;
                            let pick = m.clone();
                            view! {
                                <button
                                    type="button"
                                    class="facet"
                                    class:on=move || active.with(|f| *f == m)
                                    on:click=move |_| active.set(pick.clone())
                                >
                                    {m.clone()}
                                    <span class="count">{n}</span>
                                </button>
                            }
                        }
                    </For>
                </div>
            </div>
        }
    };
    view! {
        <div class="api-facets">
            {picker("Group", "Any group", facets.groups, facets.group)}
            {picker("Method", "Any method", facets.methods, facets.method)}
        </div>
    }
}
