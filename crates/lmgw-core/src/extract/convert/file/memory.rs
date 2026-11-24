//! What a file's poppler runs said of their memory (`pdf::wide`): every run
//! that was run again under the wide limit is one `memory_limit_widened`
//! warning per tool and limit, every JPEG 2000 image drawn blank one
//! `image_not_decoded` warning, each with its pages (none: the file).

use lmgw_api_types::documents::convert::{ConvertWarning, WarningCode};

use super::super::pictures::pages_said;
use crate::extract::pdf::wide::Note;

/// The file's warnings for `notes`, alike notes (the same kind, tool,
/// limit and why) one warning with their pages together, in the order
/// first said: single pages as "on pages 3, 7", runs over several pages as
/// "in the runs over pages 1–64" (poppler does not say which of a run's
/// pages holds the image), the file's own as "for the file".
pub(super) fn warnings(notes: Vec<Note>) -> Vec<ConvertWarning> {
    // Each group: its note, its single pages, its spans, whether the file.
    type Group = (Note, Vec<u32>, Vec<(u32, u32)>, bool);
    let mut groups: Vec<Group> = Vec::new();
    for n in notes {
        let i = match groups.iter().position(|(g, ..)| alike(g, &n)) {
            Some(i) => i,
            None => {
                groups.push((n.clone(), Vec::new(), Vec::new(), false));
                groups.len() - 1
            }
        };
        let g = &mut groups[i];
        match n.span() {
            None => g.3 = true,
            Some((a, b)) if a == b => g.1.push(a),
            Some(span) => g.2.push(span),
        }
    }
    groups
        .into_iter()
        .map(|(n, mut pages, mut spans, whole)| {
            pages.sort_unstable();
            pages.dedup();
            spans.sort_unstable();
            spans.dedup();
            let mut said = Vec::new();
            if whole {
                said.push("for the file".to_string());
            }
            if !pages.is_empty() {
                said.push(format!("on {}", pages_said(&pages)));
            }
            if !spans.is_empty() {
                let runs: Vec<String> = spans.iter().map(|(a, b)| format!("{a}–{b}")).collect();
                said.push(format!(
                    "in the {} over pages {}",
                    if spans.len() == 1 { "run" } else { "runs" },
                    runs.join(", ")
                ));
            }
            let mut parts: Vec<u32> = pages;
            parts.extend(spans.iter().flat_map(|&(a, b)| a..=b));
            parts.sort_unstable();
            parts.dedup();
            let code = match n {
                Note::Widened { .. } => WarningCode::MemoryLimitWidened,
                Note::Undecoded { .. } => WarningCode::ImageNotDecoded,
            };
            ConvertWarning {
                code,
                message: n.said(&said.join(" and ")),
                tool: Some("poppler".to_string()),
                parts,
                option: None,
            }
        })
        .collect()
}

/// The same kind of note, of the same tool, under the same limit and why.
fn alike(a: &Note, b: &Note) -> bool {
    match (a, b) {
        (
            Note::Widened {
                tool, from, why, ..
            },
            Note::Widened {
                tool: t,
                from: f,
                why: w,
                ..
            },
        ) => (tool, from, why) == (t, f, w),
        (
            Note::Undecoded {
                tool, limit, why, ..
            },
            Note::Undecoded {
                tool: t,
                limit: l,
                why: w,
                ..
            },
        ) => (tool, limit, why) == (t, l, w),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::pdf::wide::Why;
    use crate::extract::pdf::Limit;

    #[test]
    fn alike_notes_are_one_warning_with_their_pages() {
        let crop = |p| Note::Widened {
            tool: "pdftoppm",
            from: Limit::Image,
            why: Why::Jpx,
            span: Some((p, p)),
        };
        let blank = Note::Undecoded {
            tool: "pdftoppm",
            limit: Limit::Wide,
            why: crate::extract::pdf::wide::Blank::TooLarge,
            span: Some((5, 5)),
        };
        // A run over 64 pages (the layout model's squares).
        let squares = Note::Widened {
            tool: "pdftoppm",
            from: Limit::Image,
            why: Why::Jpx,
            span: Some((1, 64)),
        };
        let text = Note::Widened {
            tool: "pdftotext",
            from: Limit::Text,
            why: Why::Memory,
            span: None,
        };
        let w = warnings(vec![crop(7), crop(3), blank, text, crop(3)]);
        assert_eq!(w.len(), 3, "{w:?}");
        assert_eq!(w[0].code, WarningCode::MemoryLimitWidened);
        assert_eq!(w[0].parts, [3, 7]);
        let m = &w[0].message;
        assert!(
            m.contains(
                "JPEG 2000 image on pages 3, 7 within POPPLER_IMAGE_MEMORY_LIMIT_BYTES \
                        (2.0 GiB)"
            ) && m.contains("run again under POPPLER_WIDE_MEMORY_LIMIT_BYTES (4.0 GiB)"),
            "{m}"
        );
        // A run's span is said as the run's, never as the image's pages.
        let w2 = warnings(vec![crop(70), squares]);
        assert_eq!(w2.len(), 1, "{w2:?}");
        assert!(
            w2[0]
                .message
                .contains("image on page 70 and in the run over pages 1–64 within"),
            "{}",
            w2[0].message
        );
        assert_eq!(w2[0].parts.len(), 65);
        assert_eq!(w[1].code, WarningCode::ImageNotDecoded);
        assert!(w[1].message.contains("drew it blank"), "{}", w[1].message);
        assert_eq!(
            (w[2].parts.len(), w[2].tool.as_deref()),
            (0, Some("poppler"))
        );
        assert!(
            w[2].message.contains(
                "pdftotext needed more than POPPLER_TEXT_MEMORY_LIMIT_BYTES \
                                   (512 MiB) of memory for the file"
            ),
            "{}",
            w[2].message
        );
    }
}
