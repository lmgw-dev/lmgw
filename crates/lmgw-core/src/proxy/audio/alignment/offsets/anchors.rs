//! The words of the answer that sit in the transcript beyond doubt: a
//! sequence diff of the two token lists, whose equal runs are the anchors.

use std::collections::HashMap;
use std::ops::Range;

use similar::{capture_diff_slices, Algorithm, DiffOp};

use super::stream::Stream;

/// For each of `words`, the range of `hay_tokens` indices its tokens equal,
/// when every token of the word equals one and they are neighbours in the
/// transcript; `None` for a word with no letters or digits and for one the
/// diff could not place whole (the gap matcher takes those).
///
/// The diff is Myers' over token ids: near-identical sequences — the
/// ordinary case, a transcript and the words read out of it — cost about
/// their length, and only the differing stretches cost more.
pub(super) fn anchors(
    hay: &Stream,
    hay_tokens: &[Range<usize>],
    words: &[Stream],
) -> Vec<Option<Range<usize>>> {
    let mut ids: HashMap<String, u32> = HashMap::new();
    let mut id = |token: String| {
        let next = ids.len() as u32;
        *ids.entry(token).or_insert(next)
    };
    let b: Vec<u32> = hay_tokens.iter().map(|t| id(hay.text(t.clone()))).collect();
    // The tokens of every word in turn, and which of them are whose.
    let mut a = Vec::new();
    let owned: Vec<Range<usize>> = words
        .iter()
        .map(|w| {
            let first = a.len();
            a.extend(w.tokens().into_iter().map(|t| id(w.text(t))));
            first..a.len()
        })
        .collect();

    let mut placed: Vec<Option<usize>> = vec![None; a.len()];
    for op in capture_diff_slices(Algorithm::Myers, &a, &b) {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            for k in 0..len {
                placed[old_index + k] = Some(new_index + k);
            }
        }
    }

    owned
        .into_iter()
        .map(|tokens| {
            let at = &placed[tokens];
            let first = (*at.first()?)?;
            at.iter()
                .enumerate()
                .all(|(k, p)| *p == Some(first + k))
                .then(|| first..first + at.len())
        })
        .collect()
}
