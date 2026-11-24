//! Which lines a page's regions take: the first half of docling's
//! `LayoutPostprocessor` (read 2026-11-18), over lines instead of its cells.
//!
//! 1. Tables (and document indexes, which docling reads as tables) and
//!    pictures first resolve overlaps among their own kind; a picture over
//!    [`PICTURE_MAX_PAGE_SHARE`] of the page is no picture. Each takes the
//!    lines that lie in it by [`HOLDER_MIN_SHARE`] of their box, except a
//!    line that lies as much in a smaller regular region that is not
//!    [`OVERLAP`] inside the table or picture ([`kept_by_text`]): docling
//!    gives a picture or a table the regions inside it, so a legend half
//!    over a chart stays text.
//! 2. A text region that only wraps others is left out ([`wrappers`]): the
//!    model often answers one region over a whole column with one for each
//!    paragraph in it. It is a wrapper when two or more other regions lie
//!    in it and the innermost region of at least [`WRAPPER_MIN_COVER`] of
//!    its lines is one of them, without two of them sharing one of
//!    poppler's blocks (so an author block answered once whole and once
//!    line by line stays whole). docling keeps such a region and its
//!    paragraphs come out as one.
//! 3. Every other line goes to a regular region (text, heading, list
//!    item, caption, …) that holds at least [`LINE_MIN_SHARE`] of its box:
//!    of those within [`SHARE_SLACK`] of the largest share, and of these
//!    within the overlap rule's score slack of the best-scored, the
//!    smallest. docling gives the line to the best-scored of the largest
//!    share; the model often answers a region over two paragraphs together
//!    with one for each, scored alike, and the smaller ones are then the
//!    paragraphs (WP8 build notes). Regions left without a line are
//!    dropped.
//! 4. A region's box shrinks to its lines, and regions that then overlap
//!    ([`OVERLAP`] of either, or of their union) are one: the group's best
//!    by docling's rule keeps the group's lines. Up to [`ROUNDS`] times,
//!    until nothing changes.
//! 5. Lines no region took are orphans: each run of them in one poppler
//!    block is a text region of its own.
//!
//! Form and key-value regions only wrap other regions and are left out. A
//! table region whose lines fill less than WP3's [`TABLE_MIN_FILL`] of
//! their grid is no table: its lines are text regions, as orphans are.
//!
//! The regions are shares of the page as it is displayed (the model saw it
//! turned by its `/Rotate`), and so are poppler's line boxes: the shares
//! are taken of the displayed size `pdftohtml` gives. Without that size
//! the size `pdftotext` gives is used, which is the unturned one, so a
//! page with a line outside it is taken for a turned page whose size is
//! not known.
//!
//! A page is not laid out from its regions (an [`Implausible`]: poppler's
//! geometry does it) when there are more regions than the model has
//! queries ([`MAX_REGIONS`]), when its displayed size is not known, when
//! none is left, when more than [`ORPHAN_MAX_SHARE`] of its lines are
//! orphans, or when more than [`PICTURE_TEXT_MAX_SHARE`] of its lines lie
//! in pictures that cover more than [`PICTURE_PAGE_MIN_AREA`] of it (a slide or a poster taken for one picture would be one
//! `<!-- image -->` and its text lost to the markdown).
//!
//! [`TABLE_MIN_FILL`]: super::super::table::TABLE_MIN_FILL
//!
//! Cost: each line is compared with each region and the regions with each
//! other, and the regions are at most [`MAX_REGIONS`], so the pass is
//! linear in the page's lines.

use super::super::super::doc::Rect;
use super::super::super::layout_model::{in_points, Implausible, Label, Region};
use super::super::{Line, Page, Role};

/// The most regions a page has: the model's queries (its `labels` output
/// is `[batch, 300]`).
pub const MAX_REGIONS: usize = 300;

/// The least share of a line's box a regular region holds to take it
/// (docling's `min_overlap` for a cell).
pub const LINE_MIN_SHARE: f32 = 0.2;

/// How far under the largest share of a line a region may hold and still
/// be one of those the line can go to.
pub const SHARE_SLACK: f32 = 0.1;

/// The least share of a line's box a table or a picture holds to take it,
/// and a region to count as holding it.
pub const HOLDER_MIN_SHARE: f32 = 0.5;

/// The least share of a text region's lines whose innermost region is
/// another one inside it, for the text region to be a wrapper.
pub const WRAPPER_MIN_COVER: f32 = 0.9;

/// A picture over this share of the page is none (docling drops it).
pub const PICTURE_MAX_PAGE_SHARE: f32 = 0.9;

/// Two regions overlap when their intersection is over this share of
/// their union or of either one (docling's overlap and containment
/// thresholds).
pub const OVERLAP: f32 = 0.8;

/// More orphan lines than this share of the page's lines: the regions do
/// not describe the page.
pub const ORPHAN_MAX_SHARE: f32 = 2.0 / 3.0;

/// More lines inside pictures than this share of the page's lines: the
/// regions do not describe the page for a reader of its text (module doc).
pub const PICTURE_TEXT_MAX_SHARE: f32 = 0.5;

/// The pictures together must also cover more than this share of the
/// page's area for [`PICTURE_TEXT_MAX_SHARE`] to apply: a slide's picture
/// is the page, a paper's text-heavy figure a quarter of it.
pub const PICTURE_PAGE_MIN_AREA: f32 = 0.5;

/// How far a line may reach past the page's size when that size is not
/// known to be the displayed one, in points.
const PAGE_EDGE_SLACK_PT: f32 = 1.0;

/// How often boxes shrink and overlaps are resolved (docling's bound).
pub const ROUNDS: usize = 3;

/// docling's overlap parameters: a smaller region loses to a better-scored
/// one only when it is at most `area` times its size and scores more than
/// `score` lower.
#[derive(Clone, Copy)]
struct Prefer {
    area: f32,
    score: f32,
}

const REGULAR: Prefer = Prefer {
    area: 1.3,
    score: 0.05,
};
const PICTURE: Prefer = Prefer {
    area: 2.0,
    score: 0.3,
};
const TABLE: Prefer = Prefer {
    area: 2.0,
    score: 0.2,
};

/// A region with the lines it took (indices into the page's lines,
/// ascending).
#[derive(Debug, Clone, PartialEq)]
pub(in super::super) struct Reg {
    pub label: Label,
    pub score: f32,
    /// In points.
    pub rect: Rect,
    pub lines: Vec<usize>,
}

/// A page's regions with their lines.
#[derive(Debug, Clone, PartialEq, Default)]
pub(in super::super) struct Taken {
    pub regs: Vec<Reg>,
}

/// Whether a page is one picture for a reader of its text: more than
/// [`PICTURE_TEXT_MAX_SHARE`] of its lines inside pictures that cover more
/// than [`PICTURE_PAGE_MIN_AREA`] of its area.
fn mostly_picture(in_pictures: usize, lines: f32, picture_area: f32, page_area: f32) -> bool {
    in_pictures as f32 > PICTURE_TEXT_MAX_SHARE * lines
        && picture_area > PICTURE_PAGE_MIN_AREA * page_area
}

fn area(r: &Rect) -> f32 {
    r.width().max(0.0) * r.height().max(0.0)
}

fn inter(a: &Rect, b: &Rect) -> f32 {
    let w = a.x1.min(b.x1) - a.x0.max(b.x0);
    let h = a.y1.min(b.y1) - a.y0.max(b.y0);
    if w > 0.0 && h > 0.0 {
        w * h
    } else {
        0.0
    }
}

/// The share of `line`'s box inside `r`; a box without area counts by its
/// centre.
fn share(line: &Rect, r: &Rect) -> f32 {
    let a = area(line);
    if a > 0.0 {
        return inter(line, r) / a;
    }
    let (cx, cy) = ((line.x0 + line.x1) / 2.0, line.cy());
    if cx >= r.x0 && cx <= r.x1 && cy >= r.y0 && cy <= r.y1 {
        1.0
    } else {
        0.0
    }
}

fn overlapping(a: &Rect, b: &Rect) -> bool {
    let i = inter(a, b);
    if i <= 0.0 {
        return false;
    }
    let (aa, ab) = (area(a), area(b));
    i / (aa + ab - i) > OVERLAP || i / aa > OVERLAP || i / ab > OVERLAP
}

/// docling's `_should_prefer_cluster`.
fn prefers(c: &Reg, o: &Reg, p: Prefer) -> bool {
    let (ac, ao) = (area(&c.rect), area(&o.rect));
    if c.label == Label::ListItem && o.label == Label::Text && (1.0 - ac / ao).abs() < 0.2 {
        return true;
    }
    if c.label == Label::Code && inter(&o.rect, &c.rect) / ao > OVERLAP {
        return true;
    }
    !(ac / ao <= p.area && o.score - c.score > p.score)
}

/// Overlapping regions as one: each group's best (docling's
/// `_select_best_cluster_from_group`) with the group's lines.
fn resolve(regs: Vec<Reg>, p: Prefer) -> Vec<Reg> {
    let n = regs.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for i in 0..n {
        for j in i + 1..n {
            if overlapping(&regs[i].rect, &regs[j].rect) {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                // The smaller index stays the root: groups keep their order.
                parent[a.max(b)] = a.min(b);
            }
        }
    }
    let roots: Vec<usize> = (0..n).map(|i| find(&mut parent, i)).collect();
    // The best of each group, by its root.
    let mut best: Vec<Option<usize>> = vec![None; n];
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, r) in roots.iter().enumerate() {
        members[*r].push(i);
    }
    for group in members.iter().filter(|g| g.len() > 1) {
        let mut pick: Option<usize> = None;
        for &c in group {
            let wins = group
                .iter()
                .all(|&o| o == c || prefers(&regs[c], &regs[o], p));
            if !wins {
                continue;
            }
            pick = Some(match pick {
                None => c,
                Some(b)
                    if area(&regs[c].rect) > area(&regs[b].rect)
                        && regs[b].score - regs[c].score <= p.score =>
                {
                    c
                }
                Some(b) => b,
            });
        }
        best[group[0]] = Some(pick.unwrap_or(group[0]));
    }
    let mut lines: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut regs: Vec<Option<Reg>> = regs.into_iter().map(Some).collect();
    for (i, root) in roots.iter().enumerate() {
        if let Some(w) = best[*root] {
            if let Some(r) = regs[i].as_mut() {
                lines[w].append(&mut r.lines);
            }
            if w != i {
                regs[i] = None;
            }
        }
    }
    regs.into_iter()
        .zip(lines)
        .filter_map(|(r, mut extra)| {
            let mut r = r?;
            if !extra.is_empty() {
                r.lines.append(&mut extra);
                r.lines.sort_unstable();
                r.lines.dedup();
            }
            Some(r)
        })
        .collect()
}

/// The table or picture that takes `line`: the one holding the largest
/// share of it, at least [`HOLDER_MIN_SHARE`]; the first of equals.
fn holder_of(holders: &[Reg], line: &Rect) -> Option<usize> {
    let mut pick: Option<(usize, f32)> = None;
    for (k, r) in holders.iter().enumerate() {
        let s = share(line, &r.rect);
        if s >= HOLDER_MIN_SHARE && pick.is_none_or(|(_, b)| s > b) {
            pick = Some((k, s));
        }
    }
    pick.map(|(k, _)| k)
}

/// Whether `line`, which the table or picture `holder` would take, lies by
/// [`HOLDER_MIN_SHARE`] in a regular region smaller than `holder` that is
/// not [`OVERLAP`] inside it (module doc, step 1). Each region is asked
/// once per such line: linear in the lines.
fn kept_by_text(regular: &[Reg], holder: &Rect, line: &Rect) -> bool {
    let whole = area(holder);
    regular.iter().any(|r| {
        let a = area(&r.rect);
        a < whole
            && share(line, &r.rect) >= HOLDER_MIN_SHARE
            && inter(&r.rect, holder) / a < OVERLAP
    })
}

/// The regular region that takes `line` (module doc, step 3).
fn region_of(regs: &[Reg], line: &Rect) -> Option<usize> {
    let shares = |k: usize| share(line, &regs[k].rect);
    let most = (0..regs.len()).map(shares).fold(0.0f32, f32::max);
    if most < LINE_MIN_SHARE {
        return None;
    }
    let near = |k: &usize| {
        let s = shares(*k);
        s >= LINE_MIN_SHARE && s >= most - SHARE_SLACK
    };
    let best_score = (0..regs.len())
        .filter(near)
        .map(|k| regs[k].score)
        .fold(f32::NEG_INFINITY, f32::max);
    (0..regs.len())
        .filter(near)
        .filter(|k| regs[*k].score >= best_score - REGULAR.score)
        .min_by(|a, b| area(&regs[*a].rect).total_cmp(&area(&regs[*b].rect)))
}

/// Which of the regular regions `regs` only wrap others (module doc, step
/// 2), over the lines `free` (ascending, so a poppler block's lines follow
/// each other). Each line is compared with each region twice.
fn wrappers(regs: &[Reg], lines: &[Line], free: &[usize]) -> Vec<bool> {
    let n = regs.len();
    let areas: Vec<f32> = regs.iter().map(|r| area(&r.rect)).collect();
    // `inside[r * n + c]`: region `c` is smaller than `r` and lies in it.
    let mut inside = vec![false; n * n];
    let mut kids = vec![0u32; n];
    for r in 0..n {
        if regs[r].label != Label::Text {
            continue;
        }
        for c in 0..n {
            let within = c != r
                && areas[c] < areas[r]
                && inter(&regs[c].rect, &regs[r].rect) / areas[c] >= OVERLAP;
            if within {
                inside[r * n + c] = true;
                kids[r] += 1;
            }
        }
    }
    if kids.iter().all(|k| *k < 2) {
        return vec![false; n];
    }
    #[derive(Clone, Copy, Default)]
    struct Tally {
        lines: u32,
        owned: u32,
        /// The last owned line's block and owner.
        last: Option<(u32, usize)>,
        first_owner: Option<usize>,
        several: bool,
        /// Two regions inside it share one of poppler's blocks.
        split: bool,
    }
    let mut tally = vec![Tally::default(); n];
    let mut holding: Vec<usize> = Vec::new();
    for &i in free {
        let l = &lines[i];
        holding.clear();
        holding.extend((0..n).filter(|k| share(&l.rect, &regs[*k].rect) >= HOLDER_MIN_SHARE));
        let Some(&inner) = holding
            .iter()
            .min_by(|a, b| areas[**a].total_cmp(&areas[**b]))
        else {
            continue;
        };
        for &r in holding.iter().filter(|r| kids[**r] >= 2) {
            let t = &mut tally[r];
            t.lines += 1;
            if !inside[r * n + inner] {
                continue;
            }
            t.owned += 1;
            t.split |= t.last.is_some_and(|(b, o)| b == l.block && o != inner);
            t.last = Some((l.block, inner));
            t.several |= t.first_owner.is_some_and(|o| o != inner);
            t.first_owner.get_or_insert(inner);
        }
    }
    tally
        .iter()
        .map(|t| t.several && !t.split && t.owned as f32 >= WRAPPER_MIN_COVER * t.lines as f32)
        .collect()
}

fn shrink(regs: &mut [Reg], lines: &[Line]) {
    for r in regs {
        let boxes = r
            .lines
            .iter()
            .filter_map(|&i| lines.get(i))
            .map(|l| &l.rect);
        if let Some(rect) = Rect::union_all(boxes) {
            r.rect = rect;
        }
    }
}

/// Runs of `lines` (ascending) in one poppler block, each a text region.
fn text_regions(page: &Page, lines: &[usize], out: &mut Vec<Reg>) {
    let mut last: Option<u32> = None;
    let start = out.len();
    for &i in lines {
        let l = &page.lines[i];
        let joins = out.len() > start && last == Some(l.block);
        match out.last_mut() {
            Some(r) if joins => {
                r.lines.push(i);
                r.rect = r.rect.union(&l.rect);
            }
            _ => out.push(Reg {
                label: Label::Text,
                score: 0.0,
                rect: l.rect,
                lines: vec![i],
            }),
        }
        last = Some(l.block);
    }
}

/// `page`'s regions with their lines (module doc), from the model's `raw`
/// regions as shares of the page as displayed, `displayed` its size when
/// `pdftohtml` gave it; an [`Implausible`]: lay the page out without them.
/// The lines tables took are marked [`Role::Cell`], as poppler's geometry
/// marks its tables'.
pub(in super::super) fn take(
    page: &mut Page,
    raw: &[Region],
    displayed: Option<(f32, f32)>,
) -> Result<Taken, Implausible> {
    if raw.len() > MAX_REGIONS {
        return Err(Implausible::TooMany);
    }
    let (w, h) = displayed.unwrap_or((page.width, page.height));
    if !(w > 0.0 && h > 0.0) {
        return Err(Implausible::UnknownSize);
    }
    let outside =
        |l: &Line| l.rect.x1 > w + PAGE_EDGE_SLACK_PT || l.rect.y1 > h + PAGE_EDGE_SLACK_PT;
    if displayed.is_none() && page.lines.iter().any(outside) {
        return Err(Implausible::UnknownSize);
    }
    let page_area = w * h;
    let mut regs: Vec<Reg> = raw
        .iter()
        .filter_map(|r| {
            Region::checked(r.label, r.score, r.rect.x0, r.rect.y0, r.rect.x1, r.rect.y1)
        })
        .map(|r| Reg {
            label: r.label,
            score: r.score,
            rect: in_points(&r.rect, w, h),
            lines: Vec::new(),
        })
        .filter(|r| area(&r.rect) > 0.0)
        .collect();
    // Better-scored first: the order overlaps are resolved in.
    regs.sort_by(|a, b| b.score.total_cmp(&a.score));
    let (mut tables, mut pictures, mut regular) = (Vec::new(), Vec::new(), Vec::new());
    for r in regs {
        match r.label {
            Label::Table | Label::DocumentIndex => tables.push(r),
            Label::Picture if area(&r.rect) > PICTURE_MAX_PAGE_SHARE * page_area => {}
            Label::Picture => pictures.push(r),
            Label::Form | Label::KeyValueRegion => {}
            _ => regular.push(r),
        }
    }
    let mut holders = resolve(tables, TABLE);
    let tables = holders.len();
    holders.extend(resolve(pictures, PICTURE));
    let mut free: Vec<usize> = Vec::new();
    for (i, l) in page.lines.iter().enumerate() {
        match holder_of(&holders, &l.rect) {
            Some(k) if !kept_by_text(&regular, &holders[k].rect, &l.rect) => {
                holders[k].lines.push(i)
            }
            _ => free.push(i),
        }
    }
    let mut wraps = wrappers(&regular, &page.lines, &free).into_iter();
    regular.retain(|_| !wraps.next().unwrap_or(false));
    let mut orphans: Vec<usize> = Vec::new();
    for i in free {
        match region_of(&regular, &page.lines[i].rect) {
            Some(k) => regular[k].lines.push(i),
            None => orphans.push(i),
        }
    }
    regular.retain(|r| !r.lines.is_empty());
    for _ in 0..ROUNDS {
        shrink(&mut regular, &page.lines);
        let before = regular.len();
        regular = resolve(regular, REGULAR);
        if regular.len() == before {
            break;
        }
    }
    shrink(&mut regular, &page.lines);
    if holders.is_empty() && regular.is_empty() {
        return Err(Implausible::NoRegions);
    }
    let lines = page.lines.len() as f32;
    if orphans.len() as f32 > ORPHAN_MAX_SHARE * lines {
        return Err(Implausible::Orphans);
    }
    let in_pictures: usize = holders[tables..].iter().map(|p| p.lines.len()).sum();
    let picture_area: f32 = holders[tables..].iter().map(|p| area(&p.rect)).sum();
    if mostly_picture(in_pictures, lines, picture_area, page_area) {
        return Err(Implausible::PictureText);
    }
    // Tables too sparse for a grid: their lines are text, as orphans.
    let others = holders.split_off(tables);
    let (mut holders, sparse): (Vec<Reg>, Vec<Reg>) = holders
        .into_iter()
        .partition(|t| super::table::fills(&page.lines, &t.lines));
    let tables = holders.len();
    holders.extend(others);
    if !sparse.is_empty() {
        orphans.extend(sparse.into_iter().flat_map(|t| t.lines));
        orphans.sort_unstable();
    }
    for (k, t) in holders.iter_mut().take(tables).enumerate() {
        for &i in &t.lines {
            page.lines[i].role = Role::Cell(u32::try_from(k).unwrap_or(u32::MAX));
            t.rect = t.rect.union(&page.lines[i].rect);
        }
    }
    // Each run of orphans in one poppler block is a text region.
    text_regions(page, &orphans, &mut regular);
    holders.extend(regular);
    Ok(Taken { regs: holders })
}

#[cfg(test)]
mod tests;
