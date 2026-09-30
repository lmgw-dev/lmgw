//! Spreadsheets from an xlsx workbook, read sparsely (chat-complete design §8).
//!
//! No spreadsheet library: the file is a ZIP of XML and only the cells that
//! are *written* are read. Nothing is sized from a claim in the file — not the
//! `uniqueCount` of the shared strings, not a sheet's `dimension`, not a row
//! or column number — so a 2 KB file that puts a value at `A1` and one at
//! `XFD1048576` costs two cells, not the 17-billion-cell rectangle between.
//!
//! Values are raw: numbers as written in the file, booleans `TRUE`/`FALSE`,
//! errors as their code (`#DIV/0!`). A number in a date- or time-formatted
//! cell (the styles part, in the workbook's date system) is written as ISO
//! text by [`super::office_xlsx_dates`]. Formulas are not evaluated; the
//! cached value is what is read.

use std::collections::HashMap;

use super::office::{read_entry_if_present, walk_xml, Budget, OfficeError, Part, Tok, Zip};
use super::office_sheet::{parse_cell_ref, Cells};
use super::office_xlsx_dates::DateStyles;

pub(super) fn sheets(zip: &mut Zip<'_>, budget: &mut Budget) -> Result<Vec<Part>, OfficeError> {
    let workbook = read_entry_if_present(zip, "xl/workbook.xml", budget)?
        .ok_or(OfficeError::Missing("xlsx", "xl/workbook.xml"))?;
    let rels = read_entry_if_present(zip, "xl/_rels/workbook.xml.rels", budget)?
        .ok_or(OfficeError::Missing("xlsx", "xl/_rels/workbook.xml.rels"))?;

    // r:id -> target, for worksheets (a chartsheet has no cells).
    let mut targets: HashMap<String, String> = HashMap::new();
    walk_xml(&rels, "xl/_rels/workbook.xml.rels", |t| {
        if let Tok::Start(e) = t {
            if e.name == "Relationship" {
                let is_sheet = e.attr("Type").is_none_or(|t| t.ends_with("/worksheet"));
                if let (true, Some(id), Some(target)) = (is_sheet, e.attr("Id"), e.attr("Target")) {
                    targets.insert(id.to_string(), resolve("xl", target));
                }
            }
        }
        Ok(())
    })?;

    let mut names: Vec<(String, String)> = Vec::new();
    let mut date1904 = false;
    walk_xml(&workbook, "xl/workbook.xml", |t| {
        if let Tok::Start(e) = t {
            if e.name == "workbookPr" {
                date1904 = matches!(e.attr("date1904"), Some("1" | "true"));
            }
            if e.name == "sheet" {
                if let (Some(name), Some(id)) = (e.attr("name"), e.attr("r:id")) {
                    names.push((name.to_string(), id.to_string()));
                }
            }
        }
        Ok(())
    })?;

    let shared = shared_strings(zip, budget)?;
    let dates = DateStyles::load(zip, budget, date1904)?;
    let mut parts = Vec::new();
    for (name, id) in names {
        let Some(path) = targets.get(&id) else {
            continue;
        };
        let xml = read_entry_if_present(zip, path, budget)?.ok_or(OfficeError::Missing(
            "xlsx",
            "worksheet part named by the workbook",
        ))?;
        let cells = read_sheet(&xml, path, &shared, &dates, budget)?;
        parts.push(Part {
            heading: Some(format!("Sheet: {name}")),
            body: cells.table(budget)?,
        });
    }
    Ok(parts)
}

/// A relationship target as an archive path: relative to `base`, or absolute
/// with a leading `/`; `..` and `.` resolved.
fn resolve(base: &str, target: &str) -> String {
    let joined = match target.strip_prefix('/') {
        Some(abs) => abs.to_string(),
        None => format!("{base}/{target}"),
    };
    let mut out: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// `xl/sharedStrings.xml`: each `<si>` is the concatenation of its `<t>`
/// texts, rich-text runs (`<r>`) included, phonetic guides (`<rPh>`) not. The
/// table grows as strings are read; `uniqueCount` is never trusted.
fn shared_strings(zip: &mut Zip<'_>, budget: &mut Budget) -> Result<Vec<String>, OfficeError> {
    let Some(xml) = read_entry_if_present(zip, "xl/sharedStrings.xml", budget)? else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = Vec::new();
    let (mut in_si, mut in_t, mut rph) = (false, false, 0usize);
    let mut cur = String::new();
    walk_xml(&xml, "xl/sharedStrings.xml", |t| {
        match t {
            Tok::Start(e) => match e.name.as_str() {
                "si" => {
                    in_si = true;
                    cur.clear();
                }
                "rPh" if in_si => rph += 1,
                "t" if in_si && rph == 0 => in_t = true,
                _ => {}
            },
            Tok::End(n) => match n.as_str() {
                "si" => {
                    in_si = false;
                    // An empty string is still a stored String: it costs like a cell.
                    budget.spend(Cells::cost(&cur, 1), "the shared strings")?;
                    out.push(std::mem::take(&mut cur));
                }
                "rPh" => rph = rph.saturating_sub(1),
                "t" => in_t = false,
                _ => {}
            },
            Tok::Text(s) if in_t => cur.push_str(&s),
            Tok::Text(_) => {}
        }
        Ok(())
    })?;
    Ok(out)
}

/// One worksheet's cells: `<c r="B3" t="s|str|inlineStr|b|e|n"><v>…</v></c>`.
/// A cell without `r` follows the previous one; a row without `r` follows the
/// previous row.
fn read_sheet(
    xml: &[u8],
    part: &str,
    shared: &[String],
    dates: &DateStyles,
    budget: &mut Budget,
) -> Result<Cells, OfficeError> {
    let mut cells = Cells::default();
    let (mut row_no, mut next_row, mut next_col) = (0u64, 0u64, 0u64);
    // The open cell: (row, col, t).
    let mut cur: Option<(u64, u64, String, Option<usize>)> = None;
    let (mut v, mut inline) = (String::new(), String::new());
    let (mut in_v, mut in_is, mut in_t, mut rph) = (false, false, false, 0usize);
    walk_xml(xml, part, |t| {
        match t {
            Tok::Start(e) => match e.name.as_str() {
                "row" => {
                    row_no = e
                        .attr("r")
                        .and_then(|r| r.parse::<u64>().ok())
                        .and_then(|r| r.checked_sub(1))
                        .unwrap_or(next_row);
                    next_row = row_no.saturating_add(1);
                    next_col = 0;
                }
                "c" => {
                    let (row, col) = e
                        .attr("r")
                        .and_then(parse_cell_ref)
                        .unwrap_or((row_no, next_col));
                    let style = e.attr("s").and_then(|s| s.parse().ok());
                    cur = Some((row, col, e.attr("t").unwrap_or("n").to_string(), style));
                    next_col = col.saturating_add(1);
                    v.clear();
                    inline.clear();
                }
                "v" if cur.is_some() => in_v = true,
                "is" if cur.is_some() => in_is = true,
                "rPh" if in_is => rph += 1,
                "t" if in_is && rph == 0 => in_t = true,
                _ => {}
            },
            Tok::End(n) => match n.as_str() {
                "v" => in_v = false,
                "is" => in_is = false,
                "rPh" => rph = rph.saturating_sub(1),
                "t" => in_t = false,
                "c" => {
                    if let Some((row, col, ty, style)) = cur.take() {
                        let text: &str = match ty.as_str() {
                            "s" => v
                                .trim()
                                .parse::<usize>()
                                .ok()
                                .and_then(|i| shared.get(i))
                                .map_or("", String::as_str),
                            "inlineStr" => &inline,
                            "b" => match v.trim() {
                                "" => "",
                                "1" => "TRUE",
                                _ => "FALSE",
                            },
                            _ => &v,
                        };
                        // A number in a date-formatted cell is a date.
                        let date = match (ty.as_str(), style) {
                            ("n", Some(s)) => dates.render(s, text),
                            _ => None,
                        };
                        let text = date.as_deref().unwrap_or(text);
                        if !text.trim().is_empty() {
                            // A shared string can be referenced by any number of
                            // cells: each reference costs its full text.
                            budget.spend(Cells::cost(text, 1), "the cells of a sheet")?;
                            cells.put(row, col, text);
                        }
                    }
                }
                _ => {}
            },
            Tok::Text(s) if in_v => v.push_str(&s),
            Tok::Text(s) if in_t => inline.push_str(&s),
            Tok::Text(_) => {}
        }
        Ok(())
    })?;
    Ok(cells)
}
