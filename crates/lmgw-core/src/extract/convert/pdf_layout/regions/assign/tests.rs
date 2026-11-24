use super::*;

fn reg(label: Label, score: f32, r: (f32, f32, f32, f32), lines: &[usize]) -> Reg {
    Reg {
        label,
        score,
        rect: Rect {
            x0: r.0,
            y0: r.1,
            x1: r.2,
            y1: r.3,
        },
        lines: lines.to_vec(),
    }
}

impl Reg {
    fn rect_tuple(&self) -> (f32, f32, f32, f32) {
        (self.rect.x0, self.rect.y0, self.rect.x1, self.rect.y1)
    }
}

/// docling's `_select_best_cluster_from_group` and
/// `_should_prefer_cluster`, case by case.
#[test]
fn overlapping_regions_are_one_by_docling_s_rule() {
    let b = (10.0, 10.0, 110.0, 30.0);
    // Two answers for one box: the first (the better-scored) stays, and
    // takes the group's lines.
    let got = resolve(
        vec![
            reg(Label::Text, 0.9, b, &[0, 2]),
            reg(Label::ListItem, 0.6, (11.0, 10.0, 109.0, 30.0), &[1]),
        ],
        REGULAR,
    );
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].label, got[0].lines.as_slice()),
        (Label::Text, &[0, 1, 2][..])
    );
    // A region scoring clearly lower than one its size loses, whichever
    // comes first.
    let got = resolve(
        vec![
            reg(Label::Caption, 0.6, b, &[0]),
            reg(Label::Text, 0.9, (10.0, 10.0, 112.0, 30.0), &[1]),
        ],
        REGULAR,
    );
    assert_eq!((got.len(), got[0].label), (1, Label::Text));
    // Scored alike, the larger one.
    let got = resolve(
        vec![
            reg(Label::Caption, 0.90, b, &[0]),
            reg(Label::Text, 0.88, (10.0, 10.0, 112.0, 31.0), &[1]),
        ],
        REGULAR,
    );
    assert_eq!((got.len(), got[0].label), (1, Label::Text));
    // A list item about as large as a text region is not put aside for
    // scoring lower; code that holds another region is not either.
    let list = reg(Label::ListItem, 0.6, (11.0, 10.0, 109.0, 30.0), &[]);
    let text = reg(Label::Text, 0.9, b, &[]);
    assert!(prefers(&list, &text, REGULAR));
    assert!(!prefers(
        &reg(Label::Caption, 0.6, list.rect_tuple(), &[]),
        &text,
        REGULAR
    ));
    let code = reg(Label::Code, 0.5, b, &[]);
    let inside = reg(Label::Text, 0.95, (12.0, 12.0, 108.0, 28.0), &[]);
    assert!(prefers(&code, &inside, REGULAR));
    // Regions that only touch stay two, in their order.
    let got = resolve(
        vec![
            reg(Label::Text, 0.9, b, &[0]),
            reg(Label::Text, 0.8, (10.0, 30.0, 110.0, 50.0), &[1]),
            reg(Label::Text, 0.7, (10.0, 28.0, 110.0, 50.0), &[2]),
        ],
        REGULAR,
    );
    let lines: Vec<&[usize]> = got.iter().map(|r| r.lines.as_slice()).collect();
    assert_eq!(lines, vec![&[0][..], &[1, 2][..]]);
    assert!(resolve(Vec::new(), TABLE).is_empty());
}

#[test]
fn a_line_without_area_counts_by_its_centre() {
    let region = Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 10.0,
        y1: 10.0,
    };
    let dot = |x: f32, y: f32| Rect {
        x0: x,
        y0: y,
        x1: x,
        y1: y,
    };
    assert_eq!(share(&dot(5.0, 5.0), &region), 1.0);
    assert_eq!(share(&dot(15.0, 5.0), &region), 0.0);
    let half = Rect {
        x0: 5.0,
        y0: 0.0,
        x1: 15.0,
        y1: 10.0,
    };
    assert_eq!(share(&half, &region), 0.5);
}

#[test]
fn a_page_is_one_picture_only_by_lines_and_area() {
    // A slide: the picture is the page and holds the text.
    assert!(mostly_picture(8, 10.0, 80.0, 100.0));
    // A paper: a text-heavy figure on a quarter of the page.
    assert!(!mostly_picture(8, 10.0, 25.0, 100.0));
    // Pictures with few lines, and no pictures.
    assert!(!mostly_picture(2, 10.0, 80.0, 100.0));
    assert!(!mostly_picture(0, 10.0, 0.0, 100.0));
}
