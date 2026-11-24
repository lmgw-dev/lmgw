//! HTML in the gateway (document-conversion design §4.2, §5.2): the page
//! itself is parsed and written as markdown by lmgw's reader executable
//! ([`super::reader`], `lmgw-doc-reader`'s `html` module), never in this
//! process. What is left here is the one reading lmgw's own code does: a
//! page whose elements nest deeper than [`MAX_HTML_DEPTH`] is not written
//! as markdown by the reader (htmd writes recursively), and is read flat
//! instead by lmgw's HTML reducer (`quickdoc-core`'s `to_text`: headings,
//! prose and code in one pass over the text, no tree, no recursion), with a
//! note naming the bound.

pub use lmgw_doc_reader::wire::{MAX_HTML_DEPTH, PICTURE};

/// The page read flat (module doc). CPU-bound, linear in the page: the
/// caller runs it in a slot of the tool pool.
pub fn flat(bytes: &[u8]) -> String {
    quickdoc_core::ingest::html::to_text(&lmgw_doc_reader::charset::decode(bytes))
}

/// Why a page was read flat, in words.
pub fn flat_note(deep: u64) -> String {
    // The reader's count stops past its STOP_DEPTH (4 × MAX_HTML_DEPTH):
    // a page deeper than that is said as at least so deep (review N2).
    let stop = 4 * MAX_HTML_DEPTH as u64;
    let how = if deep > stop {
        format!("at least {deep} deep (the count stops past STOP_DEPTH, {stop})")
    } else {
        format!("{deep} deep")
    };
    format!(
        "this page was read flat: its elements nest {how}, past MAX_HTML_DEPTH \
         ({MAX_HTML_DEPTH}), so it was read as headings, paragraphs and code without its lists, \
         links and tables"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deep_page_reads_flat_in_one_pass() {
        let n = 100_000;
        let page = format!(
            "<h1>Deep</h1>{}text{}",
            "<div>".repeat(n),
            "</div>".repeat(n)
        );
        let started = std::time::Instant::now();
        let text = flat(page.as_bytes());
        assert!(text.contains("Deep") && text.contains("text"), "{text}");
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
        assert!(flat_note(100_002).contains("MAX_HTML_DEPTH (256)"));
    }
}
