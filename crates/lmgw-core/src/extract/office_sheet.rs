//! The sparse cell store and table writer both spreadsheet readers
//! ([`super::office_xlsx`], [`super::office_ods`]) fill.
//!
//! A sheet's `dimension` and a `<row r="1048576">` are claims, not sizes:
//! nothing here is allocated from them. Only cells that hold something are
//! stored, and the table written from them has the rows that exist and the
//! union of the used columns — never the bounding box.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::office::{md_cell, Budget, OfficeError};

/// What one stored cell is counted at against the [`Budget`], on top of its
/// text: the map entry, the string header and the ` | ` around it in the
/// table. An estimate of the real cost of a cell that holds one character, so
/// that a million one-character cells are not free.
pub const CELL_COST_BYTES: u64 = 64;

/// The non-empty cells of one sheet, by (row, column), both 0-based.
#[derive(Default)]
pub(super) struct Cells {
    map: BTreeMap<(u64, u64), String>,
}

impl Cells {
    /// Store `text` at (`row`, `col`). The caller has already spent the
    /// budget for it. Blank text is not a cell.
    pub fn put(&mut self, row: u64, col: u64, text: &str) {
        if !text.trim().is_empty() {
            self.map.insert((row, col), text.to_string());
        }
    }

    /// The cost of `times` copies of a cell holding `text`, saturating.
    pub fn cost(text: &str, times: u64) -> u64 {
        (text.len() as u64)
            .saturating_add(CELL_COST_BYTES)
            .saturating_mul(times)
    }

    /// The markdown table: a header of the column letters (first column the
    /// row number), then one line per row that has a cell. Empty when the
    /// sheet has none. The table's own size is spent before it is written.
    pub fn table(&self, budget: &mut Budget) -> Result<String, OfficeError> {
        if self.map.is_empty() {
            return Ok(String::new());
        }
        let cols: BTreeSet<u64> = self.map.keys().map(|(_, c)| *c).collect();
        let rows = self
            .map
            .keys()
            .map(|(r, _)| *r)
            .collect::<BTreeSet<_>>()
            .len() as u64;
        // Every row line has one ` | `-sized slot per used column, empty or not.
        let slots = rows.saturating_add(1).saturating_mul(cols.len() as u64 + 1);
        budget.spend(
            slots.saturating_mul(3),
            "the table of a sheet (rows that exist times the columns used)",
        )?;
        let mut out = String::from("| row |");
        for c in &cols {
            let _ = write!(out, " {} |", column_letters(*c));
        }
        out.push_str("\n| --- |");
        for _ in &cols {
            out.push_str(" --- |");
        }
        let flush = |out: &mut String, row: u64, line: &BTreeMap<u64, &str>| {
            let _ = write!(out, "\n| {} |", row + 1);
            for c in &cols {
                let _ = write!(
                    out,
                    " {} |",
                    line.get(c).map_or(String::new(), |v| md_cell(v))
                );
            }
        };
        let mut line: BTreeMap<u64, &str> = BTreeMap::new();
        let mut cur: Option<u64> = None;
        for ((r, c), v) in &self.map {
            if cur.is_some_and(|p| p != *r) {
                flush(&mut out, cur.unwrap_or(0), &line);
                line.clear();
            }
            cur = Some(*r);
            line.insert(*c, v.as_str());
        }
        if let Some(r) = cur {
            flush(&mut out, r, &line);
        }
        Ok(out)
    }
}

/// `0` → `A`, `25` → `Z`, `26` → `AA` — spreadsheet column letters, for any
/// column number.
pub fn column_letters(mut n: u64) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (n % 26) as u8);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// `B3` → (row 2, col 1), both 0-based. `None` for anything that is not a
/// cell reference (or overflows).
pub fn parse_cell_ref(r: &str) -> Option<(u64, u64)> {
    let split = r.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = r.split_at(split);
    if letters.is_empty() || !letters.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let mut col: u64 = 0;
    for b in letters.bytes() {
        col = col
            .checked_mul(26)?
            .checked_add(u64::from(b.to_ascii_uppercase() - b'A') + 1)?;
    }
    let row: u64 = digits.parse().ok()?;
    Some((row.checked_sub(1)?, col - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_references_round_trip() {
        for (n, l) in [
            (0, "A"),
            (25, "Z"),
            (26, "AA"),
            (701, "ZZ"),
            (702, "AAA"),
            (16383, "XFD"),
        ] {
            assert_eq!(column_letters(n), l);
            assert_eq!(parse_cell_ref(&format!("{l}7")), Some((6, n)));
        }
        assert_eq!(parse_cell_ref("B3"), Some((2, 1)));
        assert_eq!(parse_cell_ref("b3"), Some((2, 1)));
        assert_eq!(parse_cell_ref("3B"), None);
        assert_eq!(parse_cell_ref("A0"), None);
        assert_eq!(parse_cell_ref("ZZZZZZZZZZZZZZZZZZZZ1"), None);
        assert_eq!(parse_cell_ref(""), None);
    }

    #[test]
    fn the_table_is_the_used_rows_and_columns_only() {
        let mut c = Cells::default();
        c.put(0, 0, "a");
        c.put(1_048_575, 16_383, "z|z");
        c.put(1_048_575, 0, "  ");
        let t = c.table(&mut Budget::new()).unwrap();
        assert_eq!(
            t,
            "| row | A | XFD |\n| --- | --- | --- |\n| 1 | a |  |\n| 1048576 |  | z\\|z |"
        );
        assert_eq!(Cells::default().table(&mut Budget::new()).unwrap(), "");
    }
}
