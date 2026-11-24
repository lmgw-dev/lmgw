//! The layout pass on real poppler geometry: pages LibreOffice 26.8 made
//! (2026-11-18) and `pdftotext -bbox-layout` / `pdftohtml -xml` of poppler
//! 26.08 read, captured as one line per text line: `block x0 y0 x1 size
//! bold|text`. Rows of tables and label/value pages come out row by row,
//! the columns of a paper column by column, and the centred lines between
//! them (an author line, an "Abstract" heading, a caption) in their place.

use super::*;

/// The pages of a capture, blocks in order.
fn capture(text: &str) -> Vec<Vec<L>> {
    let mut blocks: Vec<(u32, Vec<L>)> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (geo, words) = line.trim().split_once('|').unwrap();
        let f: Vec<f32> = geo.split_whitespace().map(|v| v.parse().unwrap()).collect();
        let mut x = l(f[1], f[2], f[3], f[4], words);
        x.bold = f[5] > 0.0;
        match blocks.last_mut() {
            Some((b, v)) if *b == f[0] as u32 => v.push(x),
            _ => blocks.push((f[0] as u32, vec![x])),
        }
    }
    blocks.into_iter().map(|(_, v)| v).collect()
}

const TABLE3: &str = "\
1 56.8 65.8 103.7 13 1|Supplies
2 56.8 86.2 228.5 10 0|The table below lists the example supplies.
3 56.8 103.4 75.1 10 0|Item
3 56.8 115.0 79.5 10 0|Paper
3 56.8 126.5 75.7 10 0|Pens
3 56.8 138.1 70.1 10 0|Ink
3 56.8 149.6 85.1 10 0|Stapler
3 56.8 161.2 158.2 10 0|Prices are examples only.
4 217.4 103.4 252.3 10 0|Quantity
4 217.4 115.0 227.4 10 0|12
4 217.4 126.5 222.4 10 0|4
4 217.4 138.1 222.4 10 0|1
4 217.4 149.6 222.4 10 0|2
5 378.1 103.4 398.6 10 0|Price
5 378.1 115.0 395.6 10 0|3.50
5 378.1 126.5 395.6 10 0|1.20
5 378.1 138.1 395.6 10 0|9.90
5 378.1 149.6 395.6 10 0|7.00
";

const LABELS: &str = "\
1 56.8 65.8 147.4 13 1|Example Person
2 56.8 86.2 80.6 10 0|Name
2 56.8 97.8 80.7 10 0|Email
2 56.8 109.3 81.8 10 0|Phone
2 56.8 120.9 74.0 10 0|City
2 56.8 132.4 75.6 10 0|Role
2 56.8 144.0 89.5 10 0|Address
3 198.6 86.2 263.8 10 0|Example Person
3 198.6 97.8 283.9 10 0|person@example.org
3 198.6 109.3 249.2 10 0|+1 555 0100
3 198.6 120.9 253.8 10 0|Example City
3 198.6 132.4 331.7 10 0|Senior engineer at Example Corp
3 198.6 144.0 510.4 10 0|1 Example Street, Building 7, Floor 3, Example City, Example Region, 00000
3 198.6 155.5 269.3 10 0|Example Country
4 56.8 167.1 138.5 10 0|Invoice lines follow.
5 56.8 184.3 101.7 10 0|Invoice no.
5 56.8 195.8 75.6 10 0|Date
5 56.8 207.4 73.4 10 0|Due
5 56.8 218.9 77.2 10 0|Total
6 198.6 184.3 235.2 10 0|EX-0001
6 198.6 195.8 245.2 10 0|2026-01-02
6 198.6 207.4 245.2 10 0|2026-02-01
6 198.6 218.9 226.1 10 0|123.45
";

const PAPER: &str = "\
1 118.8 57.6 476.6 20 1|An Example Study of Widget Throughput
2 250.6 94.3 344.9 10 0|A. Example, B. Sample
3 277.3 117.1 318.2 11 1|Abstract
4 56.8 135.4 537.4 10 0|[abs] The example widget team reviewed the quarterly numbers and found that the throughput of the sample pipeline
4 56.8 147.0 537.2 10 0|rose steadily while the error rate fell. The example widget team reviewed the quarterly numbers and found that the
4 56.8 158.5 537.3 10 0|throughput of the sample pipeline rose steadily while the error rate fell. The example widget team reviewed the
4 56.8 170.1 491.8 10 0|quarterly numbers and found that the throughput of the sample pipeline rose steadily while the error rate fell.
5 56.8 195.9 137.0 13 1|1 Introduction
6 309.1 195.8 437.2 10 0|steadily while the error rate fell.
7 56.8 216.4 285.9 10 0|[p01] The example widget team reviewed the quarterly
7 56.8 227.9 285.9 10 0|numbers and found that the throughput of the sample
7 56.8 239.5 286.1 10 0|pipeline rose steadily while the error rate fell. The
7 56.8 251.0 286.1 10 0|example widget team reviewed the quarterly numbers
7 56.8 262.6 286.1 10 0|and found that the throughput of the sample pipeline rose
7 56.8 274.1 184.9 10 0|steadily while the error rate fell.
8 309.1 213.0 538.2 10 0|[p04] The example widget team reviewed the quarterly
8 309.1 224.5 538.2 10 0|numbers and found that the throughput of the sample
8 309.1 236.1 538.4 10 0|pipeline rose steadily while the error rate fell. The
8 309.1 247.6 538.4 10 0|example widget team reviewed the quarterly numbers
8 309.1 259.2 538.4 10 0|and found that the throughput of the sample pipeline rose
8 309.1 270.7 437.2 10 0|steadily while the error rate fell.
9 56.8 291.3 285.9 10 0|[p02] The example widget team reviewed the quarterly
9 56.8 302.9 285.9 10 0|numbers and found that the throughput of the sample
9 56.8 314.4 286.1 10 0|pipeline rose steadily while the error rate fell. The
9 56.8 326.0 286.1 10 0|example widget team reviewed the quarterly numbers
9 56.8 337.5 286.1 10 0|and found that the throughput of the sample pipeline rose
9 56.8 349.1 184.9 10 0|steadily while the error rate fell.
10 309.1 287.9 538.2 10 0|[p05] The example widget team reviewed the quarterly
10 309.1 299.5 538.2 10 0|numbers and found that the throughput of the sample
10 309.1 311.0 538.4 10 0|pipeline rose steadily while the error rate fell. The
10 309.1 322.6 538.4 10 0|example widget team reviewed the quarterly numbers
10 309.1 334.1 538.4 10 0|and found that the throughput of the sample pipeline rose
10 309.1 345.7 437.2 10 0|steadily while the error rate fell.
11 56.8 366.3 285.9 10 0|[p03] The example widget team reviewed the quarterly
11 56.8 377.8 285.9 10 0|numbers and found that the throughput of the sample
11 56.8 389.4 286.1 10 0|pipeline rose steadily while the error rate fell. The
11 56.8 400.9 286.1 10 0|example widget team reviewed the quarterly numbers
11 56.8 412.5 286.1 10 0|and found that the throughput of the sample pipeline rose
12 309.1 362.9 538.2 10 0|[p06] The example widget team reviewed the quarterly
12 309.1 374.4 538.2 10 0|numbers and found that the throughput of the sample
12 309.1 386.0 538.4 10 0|pipeline rose steadily while the error rate fell. The
12 309.1 397.5 538.4 10 0|example widget team reviewed the quarterly numbers
12 309.1 409.1 538.4 10 0|and found that the throughput of the sample pipeline rose
12 309.1 420.6 437.2 10 0|steadily while the error rate fell.
13 243.3 443.5 352.1 10 0|Figure 1: Example caption.
14 56.8 469.3 109.8 13 1|2 Method
15 309.1 469.2 437.2 10 0|steadily while the error rate fell.
16 56.8 489.8 285.9 10 0|[p07] The example widget team reviewed the quarterly
16 56.8 501.3 285.9 10 0|numbers and found that the throughput of the sample
16 56.8 512.9 286.1 10 0|pipeline rose steadily while the error rate fell. The
16 56.8 524.4 286.1 10 0|example widget team reviewed the quarterly numbers
16 56.8 536.0 286.1 10 0|and found that the throughput of the sample pipeline rose
16 56.8 547.5 184.9 10 0|steadily while the error rate fell.
17 309.1 486.4 538.2 10 0|[p09] The example widget team reviewed the quarterly
17 309.1 497.9 538.2 10 0|numbers and found that the throughput of the sample
17 309.1 509.5 538.4 10 0|pipeline rose steadily while the error rate fell. The
17 309.1 521.0 538.4 10 0|example widget team reviewed the quarterly numbers
17 309.1 532.6 538.4 10 0|and found that the throughput of the sample pipeline rose
17 309.1 544.1 437.2 10 0|steadily while the error rate fell.
18 56.8 564.7 285.9 10 0|[p08] The example widget team reviewed the quarterly
18 56.8 576.3 285.9 10 0|numbers and found that the throughput of the sample
18 56.8 587.8 286.1 10 0|pipeline rose steadily while the error rate fell. The
18 56.8 599.4 286.1 10 0|example widget team reviewed the quarterly numbers
18 56.8 610.9 286.1 10 0|and found that the throughput of the sample pipeline rose
19 309.1 561.3 538.2 10 0|[p10] The example widget team reviewed the quarterly
19 309.1 572.9 538.2 10 0|numbers and found that the throughput of the sample
19 309.1 584.4 538.4 10 0|pipeline rose steadily while the error rate fell. The
19 309.1 596.0 538.4 10 0|example widget team reviewed the quarterly numbers
19 309.1 607.5 538.4 10 0|and found that the throughput of the sample pipeline rose
19 309.1 619.1 437.2 10 0|steadily while the error rate fell.
";

/// Where each of `marks` is in `text`, which must be in that order.
fn in_order(text: &str, marks: &[&str]) {
    let mut at = 0;
    for m in marks {
        let Some(i) = text[at..].find(m) else {
            panic!("{m:?} is not after byte {at} in:\n{text}");
        };
        at += i + m.len();
    }
}

#[test]
fn a_three_column_table_without_borders_comes_out_row_by_row() {
    let lay = run(&[capture(TABLE3)]);
    assert_eq!(
        md(&lay),
        "# Supplies\n\nThe table below lists the example supplies.\n\n\
         | Item | Quantity | Price |\n| --- | --- | --- |\n| Paper | 12 | 3.50 |\n\
         | Pens | 4 | 1.20 |\n| Ink | 1 | 9.90 |\n| Stapler | 2 | 7.00 |\n\n\
         Prices are examples only."
    );
}

#[test]
fn a_label_value_page_comes_out_row_by_row() {
    let lay = run(&[capture(LABELS)]);
    let m = md(&lay);
    in_order(
        &m,
        &[
            "# Example Person",
            "| Name | Example Person |",
            "| Email | person@example.org |",
            "| Role | Senior engineer at Example Corp |",
            // The value that wraps goes on in its cell.
            "| Address | 1 Example Street, Building 7, Floor 3, Example City, Example Region, \
             00000 Example Country |",
            "Invoice lines follow.",
            "| Invoice no. | EX-0001 |",
            "| Date | 2026-01-02 |",
            "| Total | 123.45 |",
        ],
    );
    assert_eq!(lay.doc.counts().tables, 2, "{m}");
}

/// Two label/value rows are too few for a table: still read row by row.
#[test]
fn side_by_side_blocks_that_are_no_columns_read_row_by_row() {
    let few: String = LABELS
        .lines()
        .filter(|l| {
            ["Name", "Email", "Example Person", "person@"]
                .iter()
                .any(|w| l.contains(w))
        })
        .map(|l| format!("{l}\n"))
        .collect();
    let lay = run(&[capture(&few)]);
    let m = md(&lay);
    assert!(
        m.ends_with("Name Example Person\nEmail person@example.org"),
        "{m}"
    );
    assert_eq!(lay.doc.counts().tables, 0);
}

/// The paper page with its author line, "Abstract" heading and caption
/// where LibreOffice centred them, and with the author line moved across
/// the gutter or into the right half (as the review measured it): the
/// columns are read one after the other, the centred lines in their place.
#[test]
fn a_two_column_paper_reads_column_by_column_around_centred_lines() {
    for (x0, x1) in [(250.6, 344.9), (170.0, 335.0), (200.0, 308.0)] {
        let moved = PAPER.replace("2 250.6 94.3 344.9", &format!("2 {x0:.1} 94.3 {x1:.1}"));
        let lay = run(&[capture(&moved)]);
        let m = md(&lay);
        in_order(
            &m,
            &[
                "An Example Study of Widget Throughput",
                "A. Example, B. Sample",
                "Abstract",
                "[abs]",
                "1 Introduction",
                "[p01]",
                "[p02]",
                "[p03]",
                "pipeline rose\n\nsteadily while the error rate fell.\n\n[p04]",
                "[p05]",
                "[p06]",
                "Figure 1: Example caption.",
                "2 Method",
                "[p07]",
                "[p08]",
                "pipeline rose\n\nsteadily while the error rate fell.\n\n[p09]",
                "[p10]",
            ],
        );
        assert_eq!(lay.doc.counts().tables, 0, "{m}");
    }
}
