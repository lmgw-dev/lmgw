use super::*;

/// A page of one line per unit, each unit's line its own index.
fn page_of(rects: &[(f32, f32, f32, f32)]) -> (Page, Vec<Unit>) {
    let mut page = Page::default();
    let mut units = Vec::new();
    for (i, &(x0, y0, x1, y1)) in rects.iter().enumerate() {
        let rect = Rect { x0, y0, x1, y1 };
        page.lines.push(Line {
            rect,
            text: format!("u{i}"),
            size: 10.0,
            bold: false,
            block: i as u32,
            role: Role::Body,
        });
        units.push(Unit {
            rect,
            what: What::Lines(vec![i]),
        });
    }
    (page, units)
}

fn ids(u: &[Unit]) -> Vec<usize> {
    u.iter()
        .map(|u| match &u.what {
            What::Lines(l) => l[0],
            What::Rows(_) => usize::MAX - 1,
            What::Table(_) => usize::MAX,
            What::Region(k) => *k,
        })
        .collect()
}

fn ordered(rects: &[(f32, f32, f32, f32)]) -> Vec<usize> {
    let (page, units) = page_of(rects);
    ids(&order(&page, units))
}

#[test]
fn two_columns_under_a_title_read_column_by_column() {
    let u = [
        (57.0, 60.0, 540.0, 90.0),
        (57.0, 130.0, 286.0, 300.0),
        (57.0, 310.0, 286.0, 500.0),
        (310.0, 130.0, 540.0, 300.0),
        (310.0, 310.0, 540.0, 480.0),
        (57.0, 520.0, 540.0, 560.0),
        (57.0, 570.0, 286.0, 700.0),
        (310.0, 570.0, 540.0, 700.0),
    ];
    assert_eq!(ordered(&u), [0, 1, 2, 3, 4, 5, 6, 7]);
}

/// A narrow line with nothing beside it that crosses the gutter (or sits in
/// no column) separates sections; one inside a column stays in it.
#[test]
fn centred_lines_between_columns_separate_them() {
    for author in [(170.0, 335.0), (200.0, 308.0), (250.0, 345.0)] {
        let u = [
            (118.0, 57.0, 476.0, 80.0),
            (author.0, 94.0, author.1, 105.0),
            (277.0, 117.0, 318.0, 129.0),
            (57.0, 135.0, 537.0, 181.0),
            (57.0, 196.0, 286.0, 360.0),
            (309.0, 196.0, 538.0, 360.0),
            (243.0, 443.0, 352.0, 455.0),
            (57.0, 469.0, 286.0, 560.0),
            (309.0, 469.0, 538.0, 520.0),
        ];
        assert_eq!(ordered(&u), [0, 1, 2, 3, 4, 5, 6, 7, 8], "{author:?}");
    }
    // The left column goes on below where the right one ended: its last
    // block has nothing beside it, and stays in its column.
    let u = [
        (57.0, 100.0, 286.0, 300.0),
        (309.0, 100.0, 538.0, 300.0),
        (57.0, 310.0, 286.0, 500.0),
        (309.0, 310.0, 538.0, 400.0),
        (57.0, 510.0, 286.0, 700.0),
    ];
    assert_eq!(ordered(&u), [0, 2, 4, 1, 3]);
}

#[test]
fn side_by_side_bits_that_are_no_columns_read_top_to_bottom() {
    // A heading on the left, a short caption in the middle under it.
    let u = [
        (57.0, 60.0, 170.0, 80.0),
        (250.0, 100.0, 340.0, 112.0),
        (57.0, 120.0, 540.0, 400.0),
    ];
    assert_eq!(ordered(&u), [0, 1, 2]);
    // Narrow labels beside values, one row each: no rows to pair.
    let u = [
        (57.0, 60.0, 100.0, 70.0),
        (400.0, 60.0, 540.0, 70.0),
        (57.0, 80.0, 100.0, 90.0),
        (400.0, 80.0, 540.0, 90.0),
    ];
    assert_eq!(ordered(&u), [0, 1, 2, 3]);
}

/// 20 000 units in one row and 20 000 stacked on one spot: sorted and
/// swept, never units × units.
#[test]
fn many_units_stay_n_log_n() {
    let row: Vec<_> = (0..20_000)
        .map(|i| (i as f32 * 3.0, 100.0, i as f32 * 3.0 + 2.0, 110.0))
        .collect();
    let stacked: Vec<_> = (0..20_000).map(|_| (57.0, 100.0, 300.0, 110.0)).collect();
    for u in [row, stacked] {
        let started = std::time::Instant::now();
        assert_eq!(ordered(&u).len(), 20_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}
