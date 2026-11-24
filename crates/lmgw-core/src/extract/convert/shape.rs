//! A converted file's markdown, shaped as the request asks (design §7.1):
//! its counts for `stats`, tables as plain lines (`tables: false`) or
//! without padding (`compact_tables`), and plain text (`output: text`).
//!
//! The markdown is what lmgw's own readers write (`office.rs`): ATX
//! headings, `- ` list items, and tables of `| a | b |` rows whose second
//! row is the `| --- |` separator, a `|` inside a cell escaped as `\|`.

/// What `stats` counts in a markdown text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Headings per level, `#` to `######`.
    pub headings: [u32; 6],
    pub tables: u32,
    /// Cells of every table row but the separator.
    pub table_cells: u32,
}

/// The heading level of `line` (`## x` → 2), if it is one.
pub(super) fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    let rest = &line[hashes..];
    ((1..=6).contains(&hashes) && (rest.is_empty() || rest.starts_with(' '))).then_some(hashes)
}

/// The cells of a table row (`| a | b \| c |` → `a`, `b | c`), or `None`
/// for a line that is not one.
pub(super) fn cells(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    if t.len() < 2 || !t.starts_with('|') || !t.ends_with('|') || t.ends_with("\\|") {
        return None;
    }
    let inner = &t[1..t.len() - 1];
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '|' => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    Some(out)
}

pub(super) fn is_separator(c: &[String]) -> bool {
    !c.is_empty()
        && c.iter()
            .all(|x| x.len() >= 3 && x.trim_matches(':').chars().all(|ch| ch == '-'))
}

/// A line of `md`, as tables see it.
enum Line<'a> {
    Text(&'a str),
    /// A table's row: its cells.
    Row(Vec<String>),
    /// A table's separator, with its column count.
    Separator(usize),
}

/// A markdown text with its lines classified once: counted, and written
/// back in the forms the request asks for, without reading the lines
/// again.
pub struct Shaped<'a> {
    lines: Vec<Line<'a>>,
}

impl<'a> Shaped<'a> {
    /// `md`'s lines, each table's rows told apart: a table is a run of row
    /// lines whose second line is a separator. One pass: every line's cells
    /// are read once and the run ends are found from the back, so a long run
    /// of `|…|` lines that is no table costs its length, not its square.
    pub fn new(md: &'a str) -> Self {
        let all: Vec<&str> = md.lines().collect();
        let mut parsed: Vec<Option<Vec<String>>> = all.iter().map(|l| cells(l)).collect();
        // `run_end[i]`: where the run of row lines that `i` is in ends.
        let mut run_end = vec![all.len(); all.len() + 1];
        for i in (0..all.len()).rev() {
            run_end[i] = if parsed[i].is_none() {
                i
            } else {
                run_end[i + 1]
            };
        }
        let mut out = Vec::with_capacity(all.len());
        let mut i = 0;
        while i < all.len() {
            let end = run_end[i];
            if end - i >= 2 && parsed[i + 1].as_deref().is_some_and(is_separator) {
                for (k, c) in parsed[i..end].iter_mut().enumerate() {
                    let c = c.take().unwrap_or_default();
                    out.push(if k == 1 {
                        Line::Separator(c.len())
                    } else {
                        Line::Row(c)
                    });
                }
                i = end;
            } else {
                out.push(Line::Text(all[i]));
                i += 1;
            }
        }
        Self { lines: out }
    }

    /// What `stats` counts.
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for l in &self.lines {
            match l {
                Line::Text(t) => {
                    if let Some(n) = heading_level(t) {
                        c.headings[n - 1] += 1;
                    }
                }
                Line::Separator(_) => c.tables += 1,
                Line::Row(cells) => c.table_cells += cells.len() as u32,
            }
        }
        c
    }

    /// Write the text back, each line by `text`, each table row by `row`
    /// (`None` drops the separator).
    fn write(
        &self,
        text: impl Fn(&str) -> &str,
        row: impl Fn(&[String], &mut String),
        sep: impl Fn(usize) -> Option<String>,
    ) -> String {
        let mut out = String::new();
        let mut first = true;
        for l in &self.lines {
            let mut line = |out: &mut String| {
                if !first {
                    out.push('\n');
                }
                first = false;
            };
            match l {
                Line::Text(t) => {
                    line(&mut out);
                    out.push_str(text(t));
                }
                Line::Row(c) => {
                    line(&mut out);
                    row(c, &mut out);
                }
                Line::Separator(n) => {
                    if let Some(s) = sep(*n) {
                        line(&mut out);
                        out.push_str(&s);
                    }
                }
            }
        }
        out
    }

    /// `tables: false`: each table row a line of its cells separated by
    /// tabs, the separator left out.
    pub fn tables_as_lines(&self) -> String {
        self.write(|t| t, |c, out| out.push_str(&c.join("\t")), |_| None)
    }

    /// `compact_tables`: `|a|b|` rows, the separator `|---|---|`.
    pub fn compact_tables(&self) -> String {
        self.write(
            |t| t,
            |c, out| {
                out.push('|');
                for cell in c {
                    out.push_str(&cell.replace('|', "\\|"));
                    out.push('|');
                }
            },
            |n| Some(format!("|{}|", vec!["---"; n].join("|"))),
        )
    }

    /// `output: text`: headings without their `#`, tables as lines of cells
    /// separated by tabs; list markers and the rest as they are.
    pub fn plain(&self) -> String {
        self.write(
            |t| match heading_level(t) {
                Some(n) => t[n..].trim_start(),
                None => t,
            },
            |c, out| out.push_str(&c.join("\t")),
            |_| None,
        )
    }
}

/// What `stats` counts in `md`.
#[cfg(test)]
pub fn counts(md: &str) -> Counts {
    Shaped::new(md).counts()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Example Report\n\nIntro.\n\n## Costs\n\n| Item | Cost |\n| --- | --- |\n| Paper \\| ink | 3 |\n| Pens | 2 |\n\n- one\n- two\n\n### Notes\n#not-a-heading";

    #[test]
    fn headings_tables_and_cells_are_counted() {
        let c = counts(DOC);
        assert_eq!(c.headings, [1, 1, 1, 0, 0, 0]);
        assert_eq!(c.tables, 1);
        // Header and two body rows, two cells each.
        assert_eq!(c.table_cells, 6);
        // A run of pipes without a separator is no table.
        assert_eq!(counts("| a |\n| b |").tables, 0);
    }

    /// 100 000 pipe lines with no separator row: each line is read once, not
    /// once per line after it (that took minutes). The bound is generous;
    /// a linear pass takes milliseconds.
    #[test]
    fn a_long_run_of_pipe_lines_without_a_separator_is_linear() {
        let md = "| a |\n".repeat(100_000);
        let started = std::time::Instant::now();
        let shaped = Shaped::new(&md);
        let c = shaped.counts();
        let _ = shaped.plain();
        assert_eq!((c.tables, c.table_cells), (0, 0));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
        // The same run under a separator is one table of 100 000 rows.
        let md = format!("| a |\n| --- |\n{md}");
        assert_eq!(Shaped::new(&md).counts().tables, 1);
    }

    #[test]
    fn tables_become_lines_of_cells_or_compact_rows() {
        let lines = Shaped::new(DOC).tables_as_lines();
        assert!(
            lines.contains("Item\tCost\nPaper | ink\t3\nPens\t2"),
            "{lines}"
        );
        assert!(!lines.contains("---"), "{lines}");
        let compact = Shaped::new(DOC).compact_tables();
        assert!(
            compact.contains("|Item|Cost|\n|---|---|\n|Paper \\| ink|3|"),
            "{compact}"
        );
        assert_eq!(counts(&compact).tables, 1);
    }

    #[test]
    fn plain_text_drops_the_markup_but_keeps_the_words() {
        let t = Shaped::new(DOC).plain();
        assert!(t.starts_with("Example Report\n\nIntro.\n\nCosts\n"), "{t}");
        assert!(t.contains("Pens\t2") && t.contains("- one") && t.contains("Notes"));
        assert!(t.contains("#not-a-heading"), "{t}");
    }
}
