//! The operation rail and its narrow-width `Select` fallback (api-docs
//! design §6.2, §6.4): `page.rs` already hands this the filtered list in
//! rail order (`ApiDoc::rail_order` plus `ApiDoc::matches`) — this module is
//! presentation only, grouping consecutive same-tag rows under a head.

use leptos::prelude::*;

use crate::widgets::Select;

use super::doc::{Operation, Undocumented};

#[derive(Clone, PartialEq)]
enum RailRow {
    /// The tag group's eyebrow (§6.2: group → tag head → items).
    Group(String),
    Head {
        label: String,
        count: usize,
    },
    Op(Operation),
}

fn group(ops: &[Operation]) -> Vec<RailRow> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut last_group: Option<&str> = None;
    while i < ops.len() {
        if last_group != Some(ops[i].group.as_str()) {
            last_group = Some(ops[i].group.as_str());
            if !ops[i].group.is_empty() {
                out.push(RailRow::Group(ops[i].group.clone()));
            }
        }
        let label = ops[i].tag_name.clone();
        let j = ops[i..]
            .iter()
            .position(|o| o.tag_name != label)
            .map_or(ops.len(), |k| i + k);
        out.push(RailRow::Head {
            label,
            count: j - i,
        });
        out.extend(ops[i..j].iter().cloned().map(RailRow::Op));
        i = j;
    }
    out
}

/// The label a rail row shows: the op name for `/api/op/{name}`, method +
/// path otherwise (§6.2).
fn op_label(op: &Operation) -> String {
    op.op_name.clone().unwrap_or_else(|| op.path.clone())
}

#[component]
pub fn ApiRail(
    #[prop(into)] operations: Signal<Vec<Operation>>,
    #[prop(into)] undocumented: Signal<Vec<Undocumented>>,
    selected: RwSignal<String>,
) -> impl IntoView {
    let rows = Memo::new(move |_| group(&operations.get()));
    view! {
        <nav class="split-rail api-rail" aria-label="API operations">
            <For each=move || rows.get() key=row_key let:row>
                {render_row(row, selected)}
            </For>
            <Show when=move || !undocumented.with(Vec::is_empty)>
                <div class="rail-head">"Not documented here"</div>
                <For each=move || undocumented.get() key=|u| format!("{} {}", u.method, u.path) let:u>
                    <div class="rail-undoc" title=u.reason.clone()>
                        <span class="mono-sm">{format!("{} {}", u.method, u.path)}</span>
                    </div>
                </For>
            </Show>
        </nav>
    }
}

fn row_key(row: &RailRow) -> String {
    match row {
        RailRow::Group(label) => format!("g:{label}"),
        RailRow::Head { label, count } => format!("h:{label}:{count}"),
        RailRow::Op(op) => format!("o:{}", op.operation_id),
    }
}

fn render_row(row: RailRow, selected: RwSignal<String>) -> AnyView {
    match row {
        RailRow::Group(label) => view! { <div class="rail-group">{label}</div> }.into_any(),
        RailRow::Head { label, count } => view! {
            <div class="rail-head">{label} <span class="count">{count}</span></div>
        }
        .into_any(),
        RailRow::Op(op) => {
            let id = op.operation_id.clone();
            let pick_id = id.clone();
            let method = op.method.clone();
            let label = op_label(&op);
            let title = format!("{} {}", op.method, op.path);
            view! {
                <button
                    type="button"
                    class="rail-item op-item"
                    data-op=id.clone()
                    aria-current=move || (selected.get() == id).then_some("true")
                    title=title
                    on:click=move |_| selected.set(pick_id.clone())
                >
                    <span class=format!("method m-{}", method.to_ascii_lowercase())>{method.clone()}</span>
                    <span class="rail-label mono-sm">{label}</span>
                </button>
            }
            .into_any()
        }
    }
}

/// The `< 760px` fallback (§6.2): one flat `Select` over every filtered
/// operation, shown by the existing `.rail-select`/`.split-rail` breakpoint
/// rule — no new CSS needed for the swap itself.
#[component]
pub fn ApiRailSelect(
    #[prop(into)] operations: Signal<Vec<Operation>>,
    selected: RwSignal<String>,
) -> impl IntoView {
    let options = Signal::derive(move || {
        operations
            .get()
            .iter()
            .map(|op| {
                (
                    op.operation_id.clone(),
                    format!("{} {}", op.method, op_label(op)),
                )
            })
            .collect::<Vec<_>>()
    });
    view! {
        <div class="rail-select">
            <Select value=selected options=options placeholder="Pick an operation"/>
        </div>
    }
}
