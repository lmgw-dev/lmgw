//! The layout pass over a layout model's regions, on recorded answers: no
//! model file is needed. `example_paper.txt` is a two-page, two-column
//! paper of made-up text (`example_paper.fodt` beside it, turned into a PDF
//! by LibreOffice 26.8 on 2026-11-19: `soffice --headless --convert-to
//! pdf`), captured as poppler 26.08 reads it (`block x0 y0 x1 size
//! bold|text` a line, as `real` captures, from `pdftotext -bbox-layout`
//! and `pdftohtml -xml`) and as the pinned heron model answered it (`@
//! label score x0 y0 x1 y1`, the box as shares of the page; scores from
//! 0.3). `layout_model`'s ignored test prints a PDF's regions in that form
//! when the model file is at hand.

use super::super::super::doc::Source;
use super::super::super::layout_model::{Implausible, Label, PageRegions, Region, Regions};
use super::*;

const PAPER: &str = include_str!("example_paper.txt");

/// A capture's pages (blocks of lines) and their regions.
fn capture(text: &str) -> (Vec<Vec<Vec<L>>>, Regions) {
    let mut pages: Vec<Vec<(u32, Vec<L>)>> = Vec::new();
    let mut regions = Regions::default();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if line.trim() == "page" {
            pages.push(Vec::new());
            let n = pages.len() as u32;
            regions.pages.insert(n, PageRegions::Found(Vec::new()));
            continue;
        }
        if let Some(r) = line.strip_prefix("@ ") {
            let f: Vec<&str> = r.split_whitespace().collect();
            let v: Vec<f32> = f[1..].iter().map(|v| v.parse().unwrap()).collect();
            let label = Label::named(f[0]).unwrap();
            let n = pages.len() as u32;
            if let Some(PageRegions::Found(list)) = regions.pages.get_mut(&n) {
                list.extend(Region::checked(label, v[0], v[1], v[2], v[3], v[4]));
            }
            continue;
        }
        let (geo, words) = line.split_once('|').unwrap();
        let f: Vec<f32> = geo.split_whitespace().map(|v| v.parse().unwrap()).collect();
        let mut x = l(f[1], f[2], f[3], f[4], words);
        x.bold = f[5] > 0.0;
        let blocks = pages.last_mut().unwrap();
        match blocks.last_mut() {
            Some((b, v)) if *b == f[0] as u32 => v.push(x),
            _ => blocks.push((f[0] as u32, vec![x])),
        }
    }
    let pages = pages
        .into_iter()
        .map(|p| p.into_iter().map(|(_, v)| v).collect())
        .collect();
    (pages, regions)
}

fn run_with(pages: &[Vec<Vec<L>>], regions: &Regions) -> Layout {
    let (b, h) = xml(pages);
    layout_with(b.as_bytes(), Some(h.as_bytes()), 1, Some(regions)).unwrap()
}

/// One region; the box in points on the synthetic 595 × 842 page.
fn reg(label: Label, score: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> Region {
    Region::checked(label, score, x0 / 595.0, y0 / 842.0, x1 / 595.0, y1 / 842.0).unwrap()
}

fn one_page(regions: Vec<Region>) -> Regions {
    let mut r = Regions::default();
    r.pages.insert(1, PageRegions::Found(regions));
    r
}

fn kinds(lay: &Layout, page: usize) -> Vec<String> {
    lay.doc.parts[page]
        .blocks
        .iter()
        .map(|b| match &b.kind {
            BlockKind::Heading(n) => format!("h{n}"),
            BlockKind::Paragraph => "p".into(),
            BlockKind::ListItem => "li".into(),
            BlockKind::NumberedItem(_) => "ol".into(),
            BlockKind::Table(_) => "table".into(),
            BlockKind::Figure => "figure".into(),
            BlockKind::Caption => "caption".into(),
            BlockKind::Code => "code".into(),
            BlockKind::Formula => "formula".into(),
            BlockKind::PageHeader => "header".into(),
            BlockKind::PageFooter => "footer".into(),
        })
        .collect()
}

#[test]
fn the_example_paper_reads_as_its_regions_say() {
    let (pages, regions) = capture(PAPER);
    let lay = run_with(&pages, &regions);
    let text = md(&lay);
    // The title and the section headings, by their sizes.
    assert!(
        text.starts_with("# An Example Paper About Example Things\n\n"),
        "{text}"
    );
    for h in [
        "## Abstract",
        "## 1 Introduction",
        "## 2 Example Method",
        "## 5 Example Conclusion",
    ] {
        assert!(text.contains(&format!("\n\n{h}\n\n")), "{h}: {text}");
    }
    // Paragraphs are whole, and the two under "1 Introduction" are two.
    assert!(text.contains(
        "\n\nThis example paper exists to give a layout model something to look at. It has two \
         columns, section headings, lists, a table, a drawn figure, a formula and a piece of \
         code, and nothing in it means anything. Every name in it is an example name and every \
         number is made up for the page.\n\n"
    ), "{text}");
    assert!(
        text.contains(
            "its width.\n\nThe second paragraph lists what the example reader should look for in \
         the pages that follow:\n\n- a first example item about apples and their colours;\n\n- \
         a second example item that is long enough to wrap onto a second line of the narrow \
         column;\n\n- a third example item about pears.\n\n"
        ),
        "{text}"
    );
    // The model answered the second column's text once as a whole and once
    // for each paragraph: the three paragraphs are three.
    assert!(
        text.contains(
            "example authors therefore suggest that the example reader\n\nThe discussion goes on \
         for a while so that the text reaches the second page."
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "from the example records.\n\nA second long example paragraph makes sure of it."
        ),
        "{text}"
    );
    // Numbered items keep their numbers and are markdown's ordered items.
    assert!(
        text.contains(
            "\n\n1. Collect example values from the example table.\n\n2. Add them up with the \
         example formula.\n\n3. Print the sum with the example program.\n\n"
        ),
        "{text}"
    );
    // Code keeps its lines and their indentation.
    assert!(text.contains(
        "```\nfn example_sum(a, b, c) {\n    let s = a + b + c;\n    print(\"sum\", s);\n}\n```"
    ), "{text}");
    // The table is the model's region, its grid from the lines.
    assert!(text.contains(
        "| Fruit | Count | Weight |\n| --- | --- | --- |\n| Apple | 12 | 1.5 kg |\n| Pear | 7 | \
         0.9 kg |\n| Plum | 31 | 1.1 kg |\n| Cherry | 58 | 0.4 kg |"
    ), "{text}");
    // The drawn chart is a figure; its labels stay out of the text.
    assert!(
        text.contains("in the file for them.\n\n<!-- image -->\n\nFigure 1: Example counts"),
        "{text}"
    );
    assert!(
        !text.contains("Apple Pear") && !text.contains("\nCherry\n"),
        "{text}"
    );
    let figure = lay.doc.parts[0]
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::Figure)
        .unwrap();
    assert_eq!(figure.source, Source::Onnx);
    assert_eq!(figure.text, "Apple\nPear\nPlum\nCherry");
    assert_eq!(lay.model.pages[&1].picture_lines, 4);
    // The first column is read before the second, which starts with the table.
    let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s}: {text}"));
    assert!(at("## 3 Example Results") < at("| Fruit |"));
    assert!(at("Table 1: Example values") < at("| Fruit |"));
    assert!(at("| Cherry |") < at("## 4 Example Discussion"));
    // The running head and the page number are marked and left out.
    assert!(
        !text.contains("Example Journal") && !text.contains("of the example paper\n"),
        "{text}"
    );
    assert_eq!(kinds(&lay, 0).first().map(String::as_str), Some("header"));
    assert_eq!(kinds(&lay, 0).last().map(String::as_str), Some("footer"));
    // A paragraph cut by the page break is one block, on its first page.
    assert!(
        text.contains(
            "on the next page of this example paper, where it says a few more example words about \
         the example market, the example fruit and the example people who count it, before it \
         finally comes to its example end.\n\n## 5 Example Conclusion"
        ),
        "{text}"
    );
    assert!(lay.model.implausible.is_empty());
    assert_eq!(
        lay.model.pages.keys().copied().collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(kinds(&lay, 0).contains(&"caption".to_string()));
}

#[test]
fn without_regions_and_with_none_usable_the_pass_is_the_geometry_s() {
    let (pages, _) = capture(PAPER);
    let plain = run(&pages);
    // No answer for any page.
    let none = run_with(&pages, &Regions::default());
    assert_eq!(none.doc, plain.doc);
    assert!(none.model.pages.is_empty() && none.model.implausible.is_empty());
    // An answer without a region: the page is named, and laid out as before.
    let mut empty = Regions::default();
    empty.pages.insert(1, PageRegions::Found(Vec::new()));
    empty.pages.insert(2, PageRegions::Failed("example".into()));
    let lay = run_with(&pages, &empty);
    assert_eq!(lay.doc, plain.doc);
    assert_eq!(lay.model.implausible, vec![(1, Implausible::NoRegions)]);
    assert!(lay.model.pages.is_empty());
}

/// A page of `n` body lines in one column.
fn column(n: usize) -> Vec<Vec<L>> {
    vec![(0..n)
        .map(|i| {
            l(
                57.0,
                60.0 + 12.0 * i as f32,
                300.0,
                10.0,
                &format!("Example line {i}."),
            )
        })
        .collect()]
}

#[test]
fn regions_that_miss_most_lines_are_not_used() {
    let page = column(30);
    // One region over three of thirty lines: twenty-seven orphans.
    let few = one_page(vec![reg(Label::Text, 0.9, 50.0, 58.0, 310.0, 95.0)]);
    let lay = run_with(std::slice::from_ref(&page), &few);
    assert_eq!(lay.model.implausible, vec![(1, Implausible::Orphans)]);
    assert_eq!(lay.doc, run(std::slice::from_ref(&page)).doc);
    // Over twelve of thirty: used, the other lines a paragraph of their own.
    let some = one_page(vec![reg(Label::Text, 0.9, 50.0, 58.0, 310.0, 203.0)]);
    let lay = run_with(std::slice::from_ref(&page), &some);
    assert!(lay.model.implausible.is_empty());
    assert_eq!(kinds(&lay, 0), vec!["p", "p"]);
    let second = &lay.doc.parts[0].blocks[1];
    assert!(
        second.text.starts_with("Example line 12. Example line 13."),
        "{}",
        second.text
    );
}

#[test]
fn a_line_in_nested_regions_goes_to_the_smaller_one_scored_alike() {
    let page = column(6);
    // One region over both paragraphs and one for each, scored alike.
    let alike = one_page(vec![
        reg(Label::Text, 0.74, 50.0, 58.0, 310.0, 132.0),
        reg(Label::Text, 0.71, 50.0, 58.0, 310.0, 95.0),
        reg(Label::Text, 0.73, 50.0, 95.5, 310.0, 132.0),
    ]);
    let lay = run_with(std::slice::from_ref(&page), &alike);
    let texts: Vec<&str> = lay.doc.parts[0]
        .blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect();
    assert_eq!(
        texts,
        vec![
            "Example line 0. Example line 1. Example line 2.",
            "Example line 3. Example line 4. Example line 5."
        ]
    );
    // The outer one scored well above: its lines stay together, as docling
    // has them.
    let apart = one_page(vec![
        reg(Label::Text, 0.95, 50.0, 58.0, 310.0, 132.0),
        reg(Label::Text, 0.60, 50.0, 58.0, 310.0, 95.0),
        reg(Label::Text, 0.62, 50.0, 95.5, 310.0, 132.0),
    ]);
    let lay = run_with(std::slice::from_ref(&page), &apart);
    assert_eq!(kinds(&lay, 0), vec!["p"]);
}

#[test]
fn several_answers_for_one_box_are_one_block() {
    let page = column(2);
    // The model's second and third guesses for the same lines: the lines go
    // to the best-scored, and the answers left without a line are dropped.
    let twice = one_page(vec![
        reg(Label::Text, 0.9, 50.0, 58.0, 310.0, 84.0),
        reg(Label::ListItem, 0.6, 51.0, 58.0, 309.0, 84.0),
        reg(Label::SectionHeader, 0.5, 50.0, 58.0, 310.0, 84.5),
    ]);
    let lay = run_with(std::slice::from_ref(&page), &twice);
    assert_eq!(kinds(&lay, 0), vec!["p"]);
    assert_eq!(
        lay.doc.parts[0].blocks[0].text,
        "Example line 0. Example line 1."
    );
}

#[test]
fn columns_are_read_one_after_the_other_and_a_paragraph_goes_on_in_the_next() {
    let left = vec![
        l(57.0, 100.0, 280.0, 10.0, "The left column starts here and"),
        l(
            57.0,
            112.0,
            280.0,
            10.0,
            "its paragraph is not finished when the",
        ),
        l(57.0, 124.0, 100.0, 10.0, "column"),
    ];
    let right = vec![
        l(
            310.0,
            100.0,
            538.0,
            10.0,
            "ends, so it goes on at the top of",
        ),
        l(310.0, 112.0, 360.0, 10.0, "this one."),
        l(310.0, 136.0, 538.0, 10.0, "A new paragraph follows."),
    ];
    let title = vec![l(57.0, 60.0, 538.0, 20.0, "Example Title")];
    let regions = one_page(vec![
        reg(Label::Title, 0.9, 55.0, 58.0, 540.0, 84.0),
        reg(Label::Text, 0.9, 55.0, 98.0, 282.0, 137.0),
        reg(Label::Text, 0.9, 308.0, 98.0, 540.0, 125.0),
        reg(Label::Text, 0.9, 308.0, 134.0, 540.0, 149.0),
    ]);
    // poppler's order has the right column first.
    let lay = run_with(&[vec![right, title, left]], &regions);
    assert_eq!(
        md(&lay),
        "# Example Title\n\nThe left column starts here and its paragraph is not finished when \
         the column ends, so it goes on at the top of this one.\n\nA new paragraph follows."
    );
}

#[test]
fn a_table_region_is_a_table_whatever_its_lines_look_like() {
    // Two rows of two cells with a title line across both.
    let page = vec![
        vec![l(
            57.0,
            100.0,
            300.0,
            10.0,
            "Example amounts by example month",
        )],
        vec![
            l(57.0, 114.0, 100.0, 10.0, "March"),
            l(57.0, 126.0, 100.0, 10.0, "April"),
        ],
        vec![
            l(220.0, 114.0, 250.0, 10.0, "12"),
            l(220.0, 126.0, 250.0, 10.0, "7"),
        ],
        vec![l(57.0, 160.0, 400.0, 10.0, "A paragraph under the table.")],
    ];
    let regions = one_page(vec![
        reg(Label::Table, 0.9, 50.0, 98.0, 320.0, 140.0),
        reg(Label::Text, 0.9, 50.0, 158.0, 410.0, 173.0),
    ]);
    let lay = run_with(std::slice::from_ref(&page), &regions);
    assert_eq!(
        md(&lay),
        "| Example amounts by example month |  |\n| --- | --- |\n| March | 12 |\n| April | 7 \
         |\n\nA paragraph under the table."
    );
    let BlockKind::Table(t) = &lay.doc.parts[0].blocks[0].kind else {
        panic!("a table");
    };
    assert_eq!((t.rows, t.cols, t.cells[0].col_span), (3, 2, 2));
}

#[test]
fn formulas_footnotes_and_empty_regions() {
    let page = vec![
        vec![l(200.0, 100.0, 300.0, 10.0, "e = m c 2 (1)")],
        vec![l(57.0, 700.0, 300.0, 8.0, "1 An example footnote.")],
    ];
    let regions = one_page(vec![
        reg(Label::Formula, 0.8, 195.0, 98.0, 305.0, 113.0),
        reg(Label::Footnote, 0.8, 55.0, 698.0, 305.0, 711.0),
        // No line lies in these: a text region is dropped, a table without
        // a word is a picture of one, a picture is a figure.
        reg(Label::Text, 0.9, 55.0, 300.0, 305.0, 330.0),
        reg(Label::Table, 0.9, 55.0, 400.0, 305.0, 450.0),
        reg(Label::Picture, 0.9, 55.0, 500.0, 305.0, 600.0),
    ]);
    let lay = run_with(std::slice::from_ref(&page), &regions);
    assert_eq!(kinds(&lay, 0), vec!["formula", "figure", "figure", "p"]);
    assert_eq!(
        md(&lay),
        "$$\ne = m c 2 (1)\n$$\n\n<!-- image -->\n\n<!-- image -->\n\n1 An example footnote."
    );
    assert_eq!(lay.model.pages[&1].picture_lines, 0);
}

#[test]
fn hostile_answers_cost_nothing_and_break_nothing() {
    let page = column(40);
    let plain = run(std::slice::from_ref(&page)).doc;
    // More regions than the model has queries: not this model's answer.
    let many: Vec<Region> = (0..5000)
        .map(|i| reg(Label::Text, 0.9, 50.0, 58.0, 310.0 + (i % 7) as f32, 600.0))
        .collect();
    let started = Instant::now();
    let lay = run_with(std::slice::from_ref(&page), &one_page(many));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(lay.model.implausible, vec![(1, Implausible::TooMany)]);
    assert_eq!(lay.doc, plain);
    // Numbers that are none, boxes outside the page and without area: read
    // as no region (a recorded answer is checked like the model's own).
    let bad = [
        (f32::NAN, 0.1, 0.1, 0.5, 0.5),
        (0.9, f32::NAN, 0.1, 0.5, 0.5),
        (0.9, 0.1, f32::INFINITY, 0.5, 0.5),
        (0.9, 0.4, 0.4, 0.4, 0.9),
        (0.9, 5.0, 5.0, 9.0, 9.0),
        (0.9, -9.0, -9.0, -1.0, -1.0),
        (0.2, 0.1, 0.1, 0.5, 0.5),
    ];
    for (s, a, b, c, d) in bad {
        assert_eq!(
            Region::checked(Label::Text, s, a, b, c, d),
            None,
            "{s} {a} {b} {c} {d}"
        );
    }
    // A box far larger than the page is cut to it.
    let huge = Region::checked(Label::Text, 0.9, -1e30, -1e30, 1e30, 1e30).unwrap();
    assert_eq!((huge.rect.x0, huge.rect.x1), (0.0, 1.0));
    // Unchecked regions handed to the pass are checked there too.
    let raw = Region {
        label: Label::Text,
        score: f32::NAN,
        rect: Rect {
            x0: f32::NAN,
            y0: 0.0,
            x1: f32::INFINITY,
            y1: 1.0,
        },
    };
    let lay = run_with(std::slice::from_ref(&page), &one_page(vec![raw; 10]));
    assert_eq!(lay.model.implausible, vec![(1, Implausible::NoRegions)]);
    assert_eq!(lay.doc, plain);
}

#[test]
fn as_many_overlapping_regions_as_the_model_has_over_many_lines_stay_linear() {
    // Every query answering the same region, and its neighbours: the worst
    // the model can say. 20 000 lines on the page.
    let regions: Vec<Region> = (0..300)
        .map(|i| {
            reg(
                Label::Text,
                0.5 + (i % 50) as f32 / 100.0,
                50.0,
                10.0,
                540.0 - (i % 9) as f32,
                830.0,
            )
        })
        .collect();
    let lines: Vec<L> = (0..20_000)
        .map(|i| {
            l(
                57.0 + (i % 40) as f32 * 12.0,
                12.0 + (i / 40) as f32 * 1.6,
                67.0 + (i % 40) as f32 * 12.0,
                1.0,
                "x",
            )
        })
        .collect();
    let started = Instant::now();
    let lay = run_with(&[vec![lines]], &one_page(regions));
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert!(lay.model.implausible.is_empty());
    // Every line is in a block, whichever of the regions took it.
    let words: usize = lay.doc.parts[0]
        .blocks
        .iter()
        .map(|b| b.text.matches('x').count())
        .sum();
    assert_eq!(words, 20_000);
    assert!(lay.doc.parts[0].blocks.len() <= 300);
}

#[test]
fn a_model_page_between_geometry_pages_joins_nothing_across_them() {
    let open = vec![vec![l(
        57.0,
        100.0,
        300.0,
        10.0,
        "A paragraph that does not end and",
    )]];
    let goes_on = vec![vec![l(
        57.0,
        100.0,
        300.0,
        10.0,
        "goes on here in lower case.",
    )]];
    let text = |y: f32| reg(Label::Text, 0.9, 50.0, y - 2.0, 310.0, y + 13.0);
    // Both pages from regions: one paragraph.
    let mut both = Regions::default();
    both.pages.insert(1, PageRegions::Found(vec![text(100.0)]));
    both.pages.insert(2, PageRegions::Found(vec![text(100.0)]));
    let lay = run_with(&[open.clone(), goes_on.clone()], &both);
    assert_eq!(
        md(&lay),
        "A paragraph that does not end and goes on here in lower case."
    );
    assert!(lay.doc.parts[1].blocks.is_empty());
    // The second page by geometry: two paragraphs, as without the model.
    let mut first = Regions::default();
    first.pages.insert(1, PageRegions::Found(vec![text(100.0)]));
    let lay = run_with(&[open, goes_on], &first);
    assert_eq!(lay.doc.parts[1].blocks.len(), 1);
}

#[test]
fn a_table_s_repeated_first_row_is_no_page_header_on_model_pages() {
    // The same table at the top of four pages: its first row repeats in
    // the header band, and stays the table's.
    let page = |n: u32| {
        vec![
            vec![
                l(57.0, 40.0, 100.0, 10.0, "Item"),
                l(57.0, 52.0, 100.0, 10.0, &format!("Example {n}")),
            ],
            vec![
                l(220.0, 40.0, 260.0, 10.0, "Amount"),
                l(220.0, 52.0, 260.0, 10.0, "12"),
            ],
            vec![l(
                57.0,
                200.0,
                400.0,
                10.0,
                "A paragraph well below the table.",
            )],
        ]
    };
    let pages: Vec<_> = (1..=4).map(page).collect();
    let mut regions = Regions::default();
    for n in 1..=4 {
        regions.pages.insert(
            n,
            PageRegions::Found(vec![
                reg(Label::Table, 0.9, 50.0, 38.0, 270.0, 66.0),
                reg(Label::Text, 0.9, 50.0, 198.0, 410.0, 213.0),
            ]),
        );
    }
    let lay = run_with(&pages, &regions);
    assert!(lay.repeated.is_empty(), "{:?}", lay.repeated);
    assert!(
        md(&lay).starts_with("| Item | Amount |\n| --- | --- |\n| Example 1 | 12 |"),
        "{}",
        md(&lay)
    );
}
