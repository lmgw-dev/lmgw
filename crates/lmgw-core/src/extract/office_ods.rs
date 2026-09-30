//! Spreadsheets from an OpenDocument `.ods`, read sparsely (chat-complete
//! design §8).
//!
//! `content.xml` holds `table:table` (a sheet) → `table:table-row` →
//! `table:table-cell` / `table:covered-table-cell`. ODF compresses runs with
//! `table:number-rows-repeated` and `table:number-columns-repeated`, and a
//! real file ends every sheet with a run of a million empty rows of sixteen
//! thousand empty cells. Empty runs are skipped without being materialised.
//! A run of *non-empty* cells is real content: it is materialised and counted
//! against the [`Budget`] up front, so a file that claims a billion copies of
//! `x` is refused by name instead of allocated.
//!
//! Values are raw: `office:value` for numbers, percentages and currency,
//! `office:boolean-value` as `TRUE`/`FALSE`, `office:date-value` and
//! `office:time-value` as written (ISO 8601), otherwise the cell's text
//! paragraphs. No number or date formatting is applied.

use super::office::{read_entry_if_present, walk_xml, Budget, OfficeError, Part, Tok, Zip};
use super::office_sheet::Cells;

pub(super) fn sheets(zip: &mut Zip<'_>, budget: &mut Budget) -> Result<Vec<Part>, OfficeError> {
    let xml = read_entry_if_present(zip, "content.xml", budget)?
        .ok_or(OfficeError::Missing("ods", "content.xml"))?;

    let mut parts: Vec<Part> = Vec::new();
    // Nesting of `table:table` (a table inside a cell is not a sheet) and of
    // skipped subtrees (comments, tracked changes).
    let (mut table_depth, mut skip) = (0usize, 0usize);
    let mut name = String::new();
    let mut cells = Cells::default();
    let (mut row, mut col) = (0u64, 0u64);
    let mut row_repeat = 1u64;
    let mut row_cells: Vec<(u64, u64, String)> = Vec::new();
    // The open cell: its column run, and its value so far.
    let mut cell: Option<(u64, u64, Option<String>, String)> = None;
    let mut para_open = false;
    let mut paras = 0usize;
    let mut failure: Option<OfficeError> = None;

    walk_xml(&xml, "content.xml", |t| {
        if skip > 0 {
            match t {
                Tok::Start(_) => skip += 1,
                Tok::End(_) => skip -= 1,
                Tok::Text(_) => {}
            }
            return Ok(());
        }
        match t {
            Tok::Start(e) => match e.name.as_str() {
                "annotation" | "tracked-changes" => skip = 1,
                "table" => {
                    table_depth += 1;
                    if table_depth == 1 {
                        name = e.attr("table:name").unwrap_or("").to_string();
                        cells = Cells::default();
                        row = 0;
                    } else {
                        // Not a sheet: its content is left out.
                        skip = 1;
                        table_depth -= 1;
                    }
                }
                "table-row" if table_depth == 1 => {
                    row_repeat = repeat(&e, "table:number-rows-repeated");
                    col = 0;
                    row_cells.clear();
                }
                "table-cell" | "covered-table-cell" if table_depth == 1 => {
                    let n = repeat(&e, "table:number-columns-repeated");
                    let value = typed_value(&e);
                    cell = Some((col, n, value, String::new()));
                    col = col.saturating_add(n);
                    paras = 0;
                }
                "p" if cell.is_some() => {
                    if paras > 0 {
                        if let Some((_, _, _, text)) = cell.as_mut() {
                            text.push('\n');
                        }
                    }
                    paras += 1;
                    para_open = true;
                }
                "s" if para_open => push(&mut cell, " "),
                "tab" if para_open => push(&mut cell, "\t"),
                "line-break" if para_open => push(&mut cell, "\n"),
                _ => {}
            },
            Tok::End(n) => match n.as_str() {
                "p" => para_open = false,
                "table-cell" | "covered-table-cell" if table_depth == 1 => {
                    if let Some((c, n, value, text)) = cell.take() {
                        let v = value.unwrap_or(text);
                        if !v.trim().is_empty() {
                            row_cells.push((c, n, v));
                        }
                    }
                }
                "table-row" if table_depth == 1 => {
                    if !row_cells.is_empty() {
                        // The whole run is paid for before any of it exists.
                        let per_row = row_cells
                            .iter()
                            .fold(0u64, |a, (_, n, v)| a.saturating_add(Cells::cost(v, *n)));
                        if let Err(e) = budget.spend(
                            per_row.saturating_mul(row_repeat),
                            "repeated non-empty cells of a sheet",
                        ) {
                            failure = Some(e);
                            return Err(OfficeError::Xml(
                                "content.xml".into(),
                                "stopped: over budget".into(),
                            ));
                        }
                        for r in 0..row_repeat {
                            for (c, n, v) in &row_cells {
                                for k in 0..*n {
                                    cells.put(row.saturating_add(r), c.saturating_add(k), v);
                                }
                            }
                        }
                    }
                    row = row.saturating_add(row_repeat);
                }
                "table" if table_depth == 1 => {
                    table_depth = 0;
                    match std::mem::take(&mut cells).table(budget) {
                        Ok(body) => parts.push(Part {
                            heading: Some(format!("Sheet: {name}")),
                            body,
                        }),
                        Err(e) => {
                            failure = Some(e);
                            return Err(OfficeError::Xml(
                                "content.xml".into(),
                                "stopped: over budget".into(),
                            ));
                        }
                    }
                }
                _ => {}
            },
            Tok::Text(s) if para_open => push(&mut cell, &s),
            Tok::Text(_) => {}
        }
        Ok(())
    })
    .map_err(|e| failure.take().unwrap_or(e))?;
    Ok(parts)
}

fn push(cell: &mut Option<(u64, u64, Option<String>, String)>, s: &str) {
    if let Some((_, _, _, text)) = cell.as_mut() {
        text.push_str(s);
    }
}

/// A `number-…-repeated` attribute; absent or unparsable is 1.
fn repeat(e: &super::office::Elem, attr: &str) -> u64 {
    e.attr(attr)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1)
        .max(1)
}

/// The raw value of a typed cell, when its type carries one in an attribute.
fn typed_value(e: &super::office::Elem) -> Option<String> {
    match e.attr("office:value-type")? {
        "float" | "percentage" | "currency" => e.attr("office:value").map(String::from),
        "boolean" => e
            .attr("office:boolean-value")
            .map(|b| if b == "true" { "TRUE" } else { "FALSE" }.to_string()),
        "date" => e.attr("office:date-value").map(String::from),
        "time" => e.attr("office:time-value").map(String::from),
        "string" => e.attr("office:string-value").map(String::from),
        _ => None,
    }
}
