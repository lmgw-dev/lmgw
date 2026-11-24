//! Right-to-left text in reading order. `pdftotext -bbox-layout` writes a
//! line's words left to right as they sit on the page, and an Arabic or
//! Hebrew word's characters in that visual order too (checked with the WP0a
//! corpus' `right_to_left_0*.pdf`, poppler 26.08, 2026-11-18), where
//! `-layout` text applies the bidirectional reordering. Read as it is, such a
//! line is every word spelled backwards.
//!
//! A line with right-to-left words is put back in reading order:
//! - each right-to-left word's characters are reversed, a combining mark
//!   kept after the letter it belongs to;
//! - in a line with more right-to-left words than left-to-right ones, the
//!   runs of words are read from the right, a left-to-right run (a Latin
//!   name, a number) kept in its own order inside it; in a left-to-right
//!   line, a run of right-to-left words is reversed in its place.
//!
//! The base direction of a paragraph, mirrored brackets and numbers inside a
//! right-to-left word follow the Unicode algorithm only roughly; the
//! columns of a right-to-left page are still read left to right (design
//! §14, WP3, not yet). Linear in the line's characters.

/// A character of a right-to-left script (Hebrew, Arabic and their
/// presentation forms).
fn rtl_char(c: char) -> bool {
    matches!(c as u32, 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF)
}

/// A combining mark of those scripts, which stays after its letter.
fn mark(c: char) -> bool {
    matches!(c as u32, 0x0591..=0x05C7 | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06ED)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Rtl,
    Ltr,
    Neutral,
}

fn dir(word: &str) -> Dir {
    if word.chars().any(rtl_char) {
        Dir::Rtl
    } else if word.chars().any(char::is_alphanumeric) {
        Dir::Ltr
    } else {
        Dir::Neutral
    }
}

/// A right-to-left word in reading order: its characters reversed, each
/// letter followed by its marks.
fn reversed(word: &str) -> String {
    let mut clusters: Vec<(usize, usize)> = Vec::new();
    for (i, c) in word.char_indices() {
        match clusters.last_mut() {
            Some(last) if mark(c) => last.1 = i + c.len_utf8(),
            _ => clusters.push((i, i + c.len_utf8())),
        }
    }
    clusters.iter().rev().map(|&(a, z)| &word[a..z]).collect()
}

/// `words` (as poppler wrote them, left to right) in reading order.
pub(super) fn logical(words: Vec<String>) -> Vec<String> {
    let dirs: Vec<Dir> = words.iter().map(|w| dir(w)).collect();
    let rtl = dirs.iter().filter(|d| **d == Dir::Rtl).count();
    if rtl == 0 {
        return words;
    }
    let ltr = dirs.iter().filter(|d| **d == Dir::Ltr).count();
    let base_rtl = rtl > ltr;
    // A neutral word (punctuation) goes with the line's own direction.
    let is_rtl = |d: Dir| d == Dir::Rtl || (d == Dir::Neutral && base_rtl);
    // Runs of one direction, left to right.
    let mut runs: Vec<(bool, Vec<String>)> = Vec::new();
    for (w, d) in words.into_iter().zip(dirs) {
        let r = is_rtl(d);
        match runs.last_mut() {
            Some((rr, v)) if *rr == r => v.push(w),
            _ => runs.push((r, vec![w])),
        }
    }
    let read = |(r, v): (bool, Vec<String>)| -> Vec<String> {
        if r {
            v.into_iter().rev().map(|w| reversed(&w)).collect()
        } else {
            v
        }
    };
    if base_rtl {
        runs.into_iter().rev().flat_map(read).collect()
    } else {
        runs.into_iter().flat_map(read).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn right_to_left_lines_are_read_from_the_right() {
        // poppler's words of "البرمجة بلغة R و Python", left to right.
        assert_eq!(
            logical(w("Python و R ةغلب ةجمربلا")).join(" "),
            "البرمجة بلغة R و Python"
        );
        // A Latin run keeps its own order inside a right-to-left line.
        assert_eq!(
            logical(w("Machine Learning ثيدحلا ةلآلا ملعت")).join(" "),
            "تعلم الآلة الحديث Machine Learning"
        );
        // A Hebrew name in an English line is reversed in its place.
        assert_eq!(
            logical(w("Shalom םולש means peace")).join(" "),
            "Shalom שלום means peace"
        );
        // Marks stay after their letter.
        assert_eq!(
            reversed("\u{0628}\u{064E}\u{062A}"),
            "\u{062A}\u{0628}\u{064E}"
        );
        // Nothing right-to-left: as it is.
        assert_eq!(logical(w("plain words")), w("plain words"));
    }
}
