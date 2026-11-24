use super::*;

const HEAD: &str = "page   num  type   width height color comp bpc  enc interp  object ID x-ppi \
                    y-ppi size ratio\n----------------------------------------------------------\n";

fn info(pages: &[(u32, f64, f64)]) -> String {
    let mut s = format!("Pages:           {}\n", pages.len());
    for (n, w, h) in pages {
        s.push_str(&format!(
            "Page {n:>4} size:  {w} x {h} pts (A4)\nPage {n:>4} rot:   0\n"
        ));
    }
    s
}

fn a4(n: u32) -> String {
    info(&(1..=n).map(|i| (i, 595.201, 841.922)).collect::<Vec<_>>())
}

fn pages_of(g: Gated) -> Gate {
    match g {
        Gated::Pages(g) => g,
        Gated::Unsure => panic!("unsure"),
    }
}

#[test]
fn a_scan_s_pages_are_covers_and_need_no_run() {
    // ocr_test.pdf: 4960 × 7016 px at 600 ppi on A4, here on three pages.
    let mut list = HEAD.to_string();
    for p in 1..=3 {
        list.push_str(&format!(
            "   {p}     0 image    4960  7016  gray    1   8  image  yes        9  0   600   \
             600 89.7K 0.3%\n"
        ));
    }
    let g = pages_of(judge(&list, &a4(3), |_| true, 0.05));
    assert!(g.pages.is_empty());
    assert_eq!(g.left.cover, 3);
    assert_eq!(g.left_pages, [1, 2, 3]);
}

#[test]
fn a_figure_runs_its_page_and_masks_and_small_logos_do_not() {
    // 2203.01017v2: a 3690 × 2328 figure at 1200 ppi (221 × 140 pt, 6% of
    // letter), its mask; a 244 × 80 logo at 79 ppi (222 × 73 pt, 3%).
    let list = format!(
        "{HEAD}   1     0 image     244    80  icc     3   8  image  no        32  0    79    79 \
         3827B 6.5%\n   3     2 image    3690  2328  icc     3   8  image  no       158  0  1200  \
         1200  155K 0.6%\n   3     3 smask   3690  2328  gray    1   8  image  no       158  0  \
         1200  1200  155K 0.6%\n"
    );
    let sizes = info(&[(1, 612.0, 792.0), (2, 612.0, 792.0), (3, 612.0, 792.0)]);
    let g = pages_of(judge(&list, &sizes, |_| true, 0.05));
    // The logo is 3% of the page: not clearly below 5% (twice its area is
    // 6%), so its page runs too.
    assert_eq!(g.pages, [1, 3]);
    // At 10% the logo is clearly small (twice its area is 6.7%), the figure
    // (6.4%, 12.8% twice) is not.
    let g = pages_of(judge(&list, &sizes, |_| true, 0.1));
    assert_eq!(g.pages, [3]);
    assert_eq!((g.left.small, g.left_pages.clone()), (1, vec![1]));
    // Pages the request does not pick are not judged.
    let g = pages_of(judge(&list, &sizes, |n| n != 3, 0.05));
    assert_eq!(g.pages, [1]);
}

#[test]
fn unsure_rows_run_their_page() {
    // An inline image (one column for two), a low resolution, an unknown
    // type, a short line, a page without a size.
    let list = format!(
        "{HEAD}   1     0 image       4     2  rgb     3   8  image  no   [inline]       1     1   \
         24B 100%\n   2     1 image       4     2  rgb     3   8  image  no         4  0 0.960 \
         0.960   24B 100%\n   3     2 weird    4960  7016  gray    1   8  image  yes        9  0   \
         600   600 89.7K 0.3%\n   4     3 image\n   9     4 image    4960  7016  gray    1   8  \
         image  yes        9  0   600   600 89.7K 0.3%\n"
    );
    let g = pages_of(judge(&list, &a4(4), |_| true, 0.05));
    assert_eq!(g.pages, [1, 2, 3, 4, 9]);
    assert_eq!(g.left, LeftOut::default());
}

#[test]
fn a_line_without_a_page_makes_the_whole_gate_unsure() {
    let list = format!("{HEAD}Syntax Warning: example\n");
    assert_eq!(judge(&list, &a4(1), |_| true, 0.05), Gated::Unsure);
    // No rows at all: no run.
    assert_eq!(
        pages_of(judge(HEAD, &a4(1), |_| true, 0.05)),
        Gate::default()
    );
}

#[test]
fn a_cover_is_judged_turned_or_not_and_a_page_with_a_figure_runs_whole() {
    // Landscape drawn on a rotated portrait page; and a cover with a figure.
    let list = format!(
        "{HEAD}   1     0 image    7016  4960  gray    1   8  image  yes        9  0   600   600 \
         89.7K 0.3%\n   2     1 image    4960  7016  gray    1   8  image  yes        9  0   600   \
         600 89.7K 0.3%\n   2     2 image    1200   800  rgb     3   8  image  no        10  0   \
         300   300  155K 0.6%\n"
    );
    let g = pages_of(judge(&list, &a4(2), |_| true, 0.05));
    assert_eq!(g.pages, [2]);
    assert_eq!((g.left.cover, g.left_pages.clone()), (1, vec![1]));
}

#[test]
fn runs_share_the_slots_and_never_span_a_skipped_page() {
    assert_eq!(ranges(&[], 4), []);
    assert_eq!(ranges(&[3, 5, 8], 32), [(3, 3), (5, 5), (8, 8)]);
    assert_eq!(ranges(&[1, 2, 3, 4, 5, 6], 2), [(1, 3), (4, 6)]);
    assert_eq!(ranges(&[1, 2, 3, 7, 8], 1), [(1, 3), (7, 8)]);
    let all: Vec<u32> = (1..=300).collect();
    let r = ranges(&all, 32);
    assert_eq!(r.len(), 30);
    assert_eq!(r[0], (1, 10));
    assert_eq!(ranges(&[u32::MAX], 0), [(u32::MAX, u32::MAX)]);
}
