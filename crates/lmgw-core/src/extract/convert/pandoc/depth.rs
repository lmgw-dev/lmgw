//! pandoc's XML input bounded by its nesting before pandoc runs (security
//! round A, M1): pandoc 3.7's DocBook and FictionBook readers take time
//! growing with the square of the nesting (measured 2026-11-19, pandoc
//! 3.7.0.2: 5,000 nested `section`s 0.55 s, 20,000 6 to 23 s, 50,000 111 s
//! at 100 % of a core), so a 1 MiB file could hold a tool slot for minutes.
//! The other formats pandoc reads for lmgw (RTF, LaTeX, reStructuredText,
//! Org, MediaWiki, a Word file's equations) stayed under a second on the
//! same deep files, and are not scanned.
//!
//! The file's elements are counted by quick-xml's reader, which keeps
//! nothing but its own open-tag stack: past [`PANDOC_MAX_XML_DEPTH`] the
//! file fails with `too_deep`, named. That bound is lmgw's markdown
//! writer's own ([`MAX_HTML_DEPTH`]): pandoc's HTML of a deeper file would
//! be read flat anyway. Real DocBook and FictionBook nest a few dozen levels.

use lmgw_doc_reader::wire::MAX_HTML_DEPTH;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::extract::convert::caps::Format;

/// The deepest a DocBook or FictionBook file's elements may nest for
/// pandoc: [`MAX_HTML_DEPTH`], 256.
pub const PANDOC_MAX_XML_DEPTH: usize = MAX_HTML_DEPTH;

/// Whether `format` is scanned before pandoc reads it.
pub fn scanned(format: Format) -> bool {
    matches!(format, Format::DocBook | Format::Fb2)
}

/// The refusal for a file deeper than the bound: its depth as far as it was
/// read (`None` when it is within it). A file that is not well-formed XML is
/// left to pandoc, which names its own error.
pub fn too_deep(bytes: &[u8]) -> Option<String> {
    let mut reader = Reader::from_reader(bytes);
    let config = reader.config_mut();
    config.check_end_names = false;
    config.expand_empty_elements = false;
    let mut buf = Vec::new();
    let mut depth = 0usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(_)) => {
                depth += 1;
                if depth > PANDOC_MAX_XML_DEPTH {
                    return Some(format!(
                        "the file's elements nest deeper than PANDOC_MAX_XML_DEPTH ({}) levels, \
                         which pandoc's XML readers take minutes over; it was not read \
                         (too_deep)",
                        PANDOC_MAX_XML_DEPTH
                    ));
                }
            }
            Ok(Event::End(_)) => depth = depth.saturating_sub(1),
            Ok(Event::Eof) | Err(_) => return None,
            Ok(_) => {}
        }
        buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested(n: usize) -> Vec<u8> {
        format!(
            "<?xml version=\"1.0\"?><article>{}<para>x</para>{}</article>",
            "<section>".repeat(n),
            "</section>".repeat(n)
        )
        .into_bytes()
    }

    #[test]
    fn a_file_past_the_bound_is_named_and_one_within_it_is_left_to_pandoc() {
        // article and para are two of the levels.
        assert_eq!(too_deep(&nested(PANDOC_MAX_XML_DEPTH - 2)), None);
        let m = too_deep(&nested(PANDOC_MAX_XML_DEPTH)).unwrap();
        assert!(
            m.contains("PANDOC_MAX_XML_DEPTH (256)") && m.contains("too_deep"),
            "{m}"
        );
        // The review's 50,000 levels: refused at the bound, at once.
        let started = std::time::Instant::now();
        assert!(too_deep(&nested(50_000)).is_some());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // Many siblings are not depth; broken XML is pandoc's to name.
        let wide = format!("<a>{}</a>", "<b>x</b>".repeat(100_000)).into_bytes();
        assert_eq!(too_deep(&wide), None);
        assert_eq!(too_deep(b"<a><b></a"), None);
        assert!(scanned(Format::DocBook) && scanned(Format::Fb2) && !scanned(Format::Rtf));
    }
}
