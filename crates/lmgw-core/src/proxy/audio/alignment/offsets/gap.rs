//! The words the diff could not place, matched letter by letter inside the
//! stretch of transcript between two anchored words.

use super::stream::{Stream, Unit};

/// The `[start, end)` char spans of `words` inside `hay[from..to]`, in
/// order, each searched for from where the last match ended; `None` for a
/// word with no letters or digits or one the stretch does not hold.
///
/// This is what finds a word the model split or merged (`can't` against
/// `cant`) and the words of a script written without spaces, where a whole
/// sentence is one token of the transcript. A word may start where a
/// transcript word starts or right after the previous match; only a word of
/// two letters or more may start in the middle of one (a missed word in
/// unspaced text), because a single letter in the middle of a word is no
/// evidence of anything.
pub(super) fn locate_in_gap(
    hay: &[Unit],
    from: usize,
    to: usize,
    words: &[&Stream],
) -> Vec<Option<(usize, usize)>> {
    let mut at = from;
    words
        .iter()
        .map(|word| {
            let needle: Vec<char> = word.units.iter().map(|u| u.ch).collect();
            if needle.is_empty() || to < from + needle.len() {
                return None;
            }
            let fits = |i: usize| {
                hay[i..i + needle.len()]
                    .iter()
                    .map(|u| u.ch)
                    .eq(needle.iter().copied())
            };
            let starts = at..=to - needle.len();
            let found = starts
                .clone()
                .find(|&i| (hay[i].starts_token || i == at) && fits(i))
                .or_else(|| {
                    (needle.len() >= 2)
                        .then(|| starts.into_iter().find(|&i| fits(i)))
                        .flatten()
                })?;
            let last = found + needle.len() - 1;
            at = last + 1;
            Some((hay[found].from, hay[last].to))
        })
        .collect()
}
