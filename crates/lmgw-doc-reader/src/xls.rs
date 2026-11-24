//! Legacy Excel workbooks (`.xls`, BIFF5 and BIFF8) through calamine
//! (document-conversion design §4.2, §5.2): each sheet's cells that hold
//! something, with their text. The gateway writes them as the table its
//! own xlsx reader writes, under its budget.
//!
//! calamine reads the whole workbook when it is opened and builds every
//! sheet as a dense grid: a shared string is copied into every cell that
//! names it, a sheet's two far corners are a grid of everything between
//! them, and its compound-file reader follows whatever chain the file
//! gives. None of that is checked here beforehand. The file's claims are
//! paid for in this process, under the run's memory limit and timeout, and
//! a workbook that asks for more than they allow ends as its own named
//! failure with nothing of it in the gateway.

use std::io::Cursor;

use calamine::{Data, Reader, Xls};
use lmgw_doc_reader::wire::{Failure, Sheet};

/// A cell's text: numbers without a needless `.0`, dates as ISO dates.
fn text(d: &Data) -> String {
    match d {
        Data::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", *f as i64),
        Data::DateTime(dt) if dt.is_duration() => {
            let secs = (dt.as_f64() * 86_400.0).round() as i64;
            format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
        }
        Data::DateTime(dt) => match dt.as_datetime() {
            Some(t) if t.time() == chrono::NaiveTime::MIN => t.date().to_string(),
            Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
            None => d.to_string(),
        },
        other => other.to_string(),
    }
}

/// The workbook's sheets, in its order.
pub fn read(bytes: &[u8]) -> Result<Vec<Sheet>, Failure> {
    let mut book = Xls::new(Cursor::new(bytes))
        .map_err(|e| Failure::Unreadable(format!("the Excel workbook could not be read: {e}")))?;
    let mut sheets = Vec::new();
    for name in book.sheet_names() {
        let range = book.worksheet_range(&name).map_err(|e| {
            Failure::Unreadable(format!("the sheet '{name}' could not be read: {e}"))
        })?;
        let (r0, c0) = range.start().unwrap_or((0, 0));
        let mut cells = Vec::new();
        for (r, c, d) in range.used_cells() {
            let t = text(d);
            if t.trim().is_empty() {
                continue;
            }
            let row = r0.saturating_add(u32::try_from(r).unwrap_or(u32::MAX));
            let col = c0.saturating_add(u32::try_from(c).unwrap_or(u32::MAX));
            cells.push((row, col, t));
        }
        sheets.push(Sheet { name, cells });
    }
    Ok(sheets)
}
