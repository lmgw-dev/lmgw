//! Legacy Excel workbooks (`.xls`, BIFF5 and BIFF8; document-conversion
//! design §4.2, §5.2) in the gateway: the workbook is parsed by lmgw's
//! reader executable ([`super::reader`], calamine in `lmgw-doc-reader`),
//! which answers each sheet's cells that hold something. Here they become
//! the markdown table lmgw's own xlsx reader writes (`office_sheet.rs`: the
//! column letters as the header, the row numbers first), under `## Sheet:
//! name`, so an `.xls` and the `.xlsx` saved from it read alike.
//!
//! The cells arrive bounded (the reader's answer is at most
//! `MAX_UNCOMPRESSED_BYTES`), and they and the tables written from them are
//! counted against one [`Budget`] of the same size, as the xlsx reader
//! counts its own. Nothing is allocated from a sheet's claimed size: only
//! the cells that came are stored.

use lmgw_doc_reader::wire::Sheet;

use crate::extract::office::{Budget, OfficeError, Part};
use crate::extract::office_sheet::Cells;

/// The sheets as parts, each headed `Sheet: name`. CPU-bound, linear in the
/// cells: the caller runs it in a slot of the tool pool.
pub fn parts(sheets: Vec<Sheet>) -> Result<Vec<Part>, String> {
    let mut budget = Budget::new();
    let mut parts = Vec::with_capacity(sheets.len());
    for sheet in sheets {
        let mut cells = Cells::default();
        for (row, col, text) in &sheet.cells {
            budget
                .spend(Cells::cost(text, 1), "the cells of a sheet")
                .map_err(|e: OfficeError| e.to_string())?;
            cells.put(u64::from(*row), u64::from(*col), text);
        }
        let body = cells.table(&mut budget).map_err(|e| e.to_string())?;
        parts.push(Part {
            heading: Some(format!("Sheet: {}", sheet.name)),
            body,
        });
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_become_the_table_the_xlsx_reader_writes() {
        let p = parts(vec![Sheet {
            name: "Costs".into(),
            cells: vec![(0, 0, "4".into()), (0, 1, "2.5".into()), (1, 0, "7".into())],
        }])
        .unwrap();
        assert_eq!(p[0].heading.as_deref(), Some("Sheet: Costs"));
        assert_eq!(
            p[0].body,
            "| row | A | B |\n| --- | --- | --- |\n| 1 | 4 | 2.5 |\n| 2 | 7 |  |"
        );
    }

    /// Two cells at opposite corners of the largest sheet are two cells and
    /// a table of two rows and two columns, never a grid of what lies
    /// between.
    #[test]
    fn far_corners_cost_their_cells_not_the_grid_between() {
        let p = parts(vec![Sheet {
            name: "Wide".into(),
            cells: vec![(0, 0, "a".into()), (65_535, 65_535, "z".into())],
        }])
        .unwrap();
        assert_eq!(p[0].body.lines().count(), 4, "{}", p[0].body);
        assert!(p[0].body.contains("| 65536 |  | z |"), "{}", p[0].body);
    }
}
