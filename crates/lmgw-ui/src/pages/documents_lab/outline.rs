//! A file's structure (`output: json`) as a readable outline (WP12): parts
//! and their blocks in reading order, each block with its kind, its level,
//! its part and how it was read; tables as tables; boxes in the expert view.
//! [`PAGE_BLOCKS`] blocks at a time ([`super::pager`]), so tens of thousands
//! of blocks never become one DOM; a table shows [`TABLE_ROWS`] rows until
//! asked for all of them, and then [`TABLE_PAGE_ROWS`] at a time with a pager
//! (a table is one block, so the block paging does not cut it).

use leptos::prelude::*;
use lmgw_api_types::documents::structure::{DocBlock, DocPart, DocTable, PartKind};

use super::pager::{block_starts, locate, page_count, PAGE_BLOCKS};
use super::words;
use crate::fmt::grouped;

/// A table's rows shown before "Show all".
const TABLE_ROWS: usize = 50;
/// The rows a page of an opened table holds.
pub(super) const TABLE_PAGE_ROWS: usize = 500;

fn wire<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// A part's line: "Page 3", "Slide 2", "Sheet Totals", "Document".
fn part_title(p: &DocPart) -> String {
    match p.kind {
        PartKind::Page => format!("Page {}", p.number),
        PartKind::Slide => format!("Slide {}", p.number),
        PartKind::Sheet => match &p.name {
            Some(n) => format!("Sheet {} · {n}", p.number),
            None => format!("Sheet {}", p.number),
        },
        PartKind::Document => "Document".into(),
    }
}

fn part_line(p: &DocPart) -> String {
    let mut s = part_title(p);
    match p.source {
        Some(src) => s.push_str(&format!(" · read from {}", words::source(&wire(&src)))),
        None => s.push_str(" · not read"),
    }
    s.push_str(&format!(
        " · {}",
        words::count(p.blocks.len() as u64, "block", "blocks")
    ));
    s
}

/// A block's kind as the outline says it: "heading 2", "list item".
fn kind_label(b: &DocBlock) -> String {
    let k = wire(&b.kind).replace('_', " ");
    match b.level {
        Some(l) => format!("{k} {l}"),
        None => k,
    }
}

/// A table's rows `from..to` as cells: each row's cells left to right, a
/// cell a span from above covers left out (also one that starts above
/// `from`), an empty place an empty cell. Only the rows asked for are built.
pub(super) fn grid_window(
    t: &DocTable,
    from: usize,
    to: usize,
) -> Vec<Vec<(String, u32, u32, bool)>> {
    let end = (t.rows as usize).min(to);
    let from = from.min(end);
    let cols = t.cols as usize;
    let mut by_row: Vec<Vec<&lmgw_api_types::documents::structure::DocCell>> =
        vec![Vec::new(); end - from];
    // How many more rows each column is covered by a cell above.
    let mut covered = vec![0u32; cols];
    for c in &t.cells {
        let r = c.row as usize;
        if r >= from {
            if let Some(row) = by_row.get_mut(r - from) {
                row.push(c);
            }
        } else {
            let reach = (r + c.row_span.max(1) as usize).saturating_sub(from);
            let span = (c.col_span.max(1) as usize).min(cols.saturating_sub(c.col as usize));
            for k in covered.iter_mut().skip(c.col as usize).take(span) {
                *k = (*k).max(reach as u32);
            }
        }
    }
    let mut out = Vec::with_capacity(by_row.len());
    for mut cells in by_row {
        cells.sort_by_key(|c| c.col);
        let mut row = Vec::new();
        let mut col = 0;
        let mut it = cells.into_iter().peekable();
        while col < cols {
            if covered[col] > 0 {
                covered[col] -= 1;
                col += 1;
                continue;
            }
            match it.peek() {
                Some(c) if c.col as usize == col => {
                    let span = (c.col_span.max(1) as usize).min(cols - col);
                    for k in covered.iter_mut().skip(col).take(span) {
                        *k = c.row_span.max(1) - 1;
                    }
                    row.push((c.text.clone(), c.row_span.max(1), span as u32, c.header));
                    it.next();
                    col += span;
                }
                _ => {
                    row.push((String::new(), 1, 1, false));
                    col += 1;
                }
            }
        }
        out.push(row);
    }
    out
}

/// A table's first `rows` rows as cells.
#[cfg(test)]
pub(super) fn grid(t: &DocTable, rows: usize) -> Vec<Vec<(String, u32, u32, bool)>> {
    grid_window(t, 0, rows)
}

#[component]
fn TableView(t: DocTable) -> impl IntoView {
    let all = RwSignal::new(false);
    let page = RwSignal::new(0usize);
    let total = t.rows as usize;
    let pages = page_count(total, TABLE_PAGE_ROWS);
    let t = StoredValue::new(t);
    view! {
        <div class="doclab-table-wrap">
            <table class="doclab-table">
                {move || {
                    let (from, to) = if all.get() {
                        let from = page.get() * TABLE_PAGE_ROWS;
                        (from, from + TABLE_PAGE_ROWS)
                    } else {
                        (0, TABLE_ROWS)
                    };
                    t.with_value(|t| grid_window(t, from, to))
                        .into_iter()
                        .map(|r| {
                            view! {
                                <tr>
                                    {r
                                        .into_iter()
                                        .map(|(text, rs, cs, head)| {
                                            if head {
                                                view! { <th rowspan=rs colspan=cs>{text}</th> }.into_any()
                                            } else {
                                                view! { <td rowspan=rs colspan=cs>{text}</td> }.into_any()
                                            }
                                        })
                                        .collect_view()}
                                </tr>
                            }
                        })
                        .collect_view()
                }}
            </table>
            {(total > TABLE_ROWS)
                .then(|| {
                    view! {
                        <div class="doclab-pagebar">
                            <button
                                class="btn ghost sm"
                                on:click=move |_| {
                                    all.update(|a| *a = !*a);
                                    page.set(0);
                                }
                            >
                                {move || {
                                    if all.get() {
                                        format!("Show the first {TABLE_ROWS} rows")
                                    } else {
                                        format!("Show all {} rows", grouped(total as u64))
                                    }
                                }}
                            </button>
                            <span class="spacer"></span>
                            {move || {
                                all.get()
                                    .then(|| {
                                        view! {
                                            <super::viewer::Pager
                                                page=page
                                                count=Signal::derive(move || pages)
                                                what="rows"
                                            />
                                        }
                                    })
                            }}
                        </div>
                    }
                })}
        </div>
    }
}

fn block_row(b: &DocBlock, part: &DocPart, expert: bool) -> AnyView {
    let kind = kind_label(b);
    let place = part_title(part);
    let how = words::source(&wire(&b.source));
    let bbox = b
        .bbox
        .filter(|_| expert)
        .map(|[a, c, d, e]| format!("[{a:.1}, {c:.1}, {d:.1}, {e:.1}]"));
    let quiet = matches!(wire(&b.kind).as_str(), "page_header" | "page_footer");
    let heading = b.level.is_some();
    view! {
        <div class="doclab-block" class:quiet=quiet class:heading=heading>
            <div class="doclab-block-meta">
                <span class="chip off doclab-kind">{kind}</span>
                <span class="dim">{place}</span>
                <span class="dim">{how}</span>
                {bbox.map(|b| view! { <span class="mono-sm dim">{b}</span> })}
            </div>
            {b.text.clone().map(|t| view! { <div class="doclab-block-text">{t}</div> })}
            {b.caption.clone().map(|c| view! { <div class="doclab-block-text dim">"Caption: "{c}</div> })}
            {b.description.clone().map(|d| {
                view! { <div class="doclab-block-text" data-description="">"Description: "{d}</div> }
            })}
            {b.table.clone().map(|t| view! { <TableView t=t/> })}
        </div>
    }
    .into_any()
}

/// The outline of `parts`.
#[component]
pub(super) fn Outline(
    parts: std::sync::Arc<Vec<DocPart>>,
    #[prop(into)] expert: Signal<bool>,
) -> impl IntoView {
    let (starts, total) = block_starts(&parts);
    let empty_parts = parts.iter().filter(|p| p.blocks.is_empty()).count();
    let n_parts = parts.len();
    let parts = StoredValue::new(parts);
    let starts = StoredValue::new(starts);
    let page = RwSignal::new(0usize);
    let count = page_count(total, PAGE_BLOCKS);
    let summary = format!(
        "{} in {}{}",
        words::count(total as u64, "block", "blocks"),
        words::count(n_parts as u64, "part", "parts"),
        if empty_parts > 0 {
            format!(" ({empty_parts} without blocks)")
        } else {
            String::new()
        }
    );
    let rows = move || {
        let p = page.get();
        let ex = expert.get();
        let from = p * PAGE_BLOCKS;
        let to = (from + PAGE_BLOCKS).min(total);
        parts.with_value(|parts| {
            starts.with_value(|starts| {
                let mut out: Vec<AnyView> = Vec::new();
                for n in from..to {
                    let Some((pi, bi)) = locate(starts, n) else {
                        break;
                    };
                    let part = &parts[pi];
                    if bi == 0 || n == from {
                        let line = part_line(part);
                        out.push(view! { <div class="doclab-part">{line}</div> }.into_any());
                    }
                    out.push(block_row(&part.blocks[bi], part, ex));
                }
                out
            })
        })
    };
    view! {
        <div class="doclab-outline">
            <div class="doclab-pagebar">
                <span class="dim">{summary}</span>
                <span class="spacer"></span>
                <super::viewer::Pager page=page count=Signal::derive(move || count) what="blocks"/>
            </div>
            {rows}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::documents::structure::DocCell;

    fn cell(row: u32, col: u32, rs: u32, cs: u32, text: &str) -> DocCell {
        DocCell {
            row,
            col,
            row_span: rs,
            col_span: cs,
            header: row == 0,
            text: text.into(),
        }
    }

    #[test]
    fn a_table_grid_fills_gaps_and_leaves_spanned_places_out() {
        let t = DocTable {
            rows: 3,
            cols: 3,
            cells: vec![
                cell(0, 0, 1, 2, "ab"),
                cell(0, 2, 2, 1, "c"),
                cell(1, 0, 1, 1, "d"),
                cell(2, 2, 1, 1, "i"),
            ],
        };
        let g = grid(&t, 10);
        let texts: Vec<Vec<&str>> = g
            .iter()
            .map(|r| r.iter().map(|c| c.0.as_str()).collect())
            .collect();
        assert_eq!(
            texts,
            vec![vec!["ab", "c"], vec!["d", ""], vec!["", "", "i"]]
        );
        assert_eq!(g[0][0].2, 2, "the column span");
        assert!(g[0][0].3, "a header cell");
        assert_eq!(grid(&t, 1).len(), 1, "only the rows shown");
        // A window that starts under a span from above leaves its places out.
        let w = grid_window(&t, 1, 3);
        let texts: Vec<Vec<&str>> = w
            .iter()
            .map(|r| r.iter().map(|c| c.0.as_str()).collect())
            .collect();
        assert_eq!(texts, vec![vec!["d", ""], vec!["", "", "i"]]);
        assert!(grid_window(&t, 5, 9).is_empty(), "past the end");
    }

    #[test]
    fn a_table_of_a_hundred_thousand_rows_builds_one_page_of_rows() {
        let rows = 100_001u32;
        let t = DocTable {
            rows,
            cols: 2,
            cells: (0..rows)
                .flat_map(|r| [cell(r, 0, 1, 1, "a"), cell(r, 1, 1, 1, "b")])
                .collect(),
        };
        let w = grid_window(&t, 99_500, 99_500 + TABLE_PAGE_ROWS);
        assert_eq!(w.len(), TABLE_PAGE_ROWS);
        let last = grid_window(&t, 100_000, 100_000 + TABLE_PAGE_ROWS);
        assert_eq!(last.len(), 1);
    }
}
