//! A page's text, decoded by its charset.

/// The page's text: a byte-order mark, else the `charset` its first bytes
/// declare (`<meta charset>` or `http-equiv`), else UTF-8 when it is valid,
/// else windows-1252, what browsers assume for a page that does not say.
pub fn decode(bytes: &[u8]) -> String {
    if let Some((enc, _)) = encoding_rs::Encoding::for_bom(bytes) {
        return enc.decode_with_bom_removal(bytes).0.into_owned();
    }
    let head = &bytes[..bytes.len().min(4096)];
    let declared =
        declared_charset(head).and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()));
    if let Some(enc) = declared {
        // A page that says UTF-16 in ASCII is not UTF-16.
        if enc != encoding_rs::UTF_16LE && enc != encoding_rs::UTF_16BE {
            return enc.decode_without_bom_handling(bytes).0.into_owned();
        }
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::WINDOWS_1252
            .decode_without_bom_handling(bytes)
            .0
            .into_owned(),
    }
}

/// `charset=…` in the first bytes, as a label.
fn declared_charset(head: &[u8]) -> Option<String> {
    let lower: Vec<u8> = head.iter().map(u8::to_ascii_lowercase).collect();
    let at = lower.windows(8).position(|w| w == b"charset=")? + 8;
    let rest = &lower[at..];
    let rest = rest
        .strip_prefix(b"\"")
        .or_else(|| rest.strip_prefix(b"'"))
        .unwrap_or(rest);
    let end = rest
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.')))
        .unwrap_or(rest.len());
    let label = String::from_utf8_lossy(&rest[..end]).into_owned();
    (!label.is_empty()).then_some(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charsets_are_read_from_the_bom_the_meta_or_the_bytes() {
        assert_eq!(decode(b"\xEF\xBB\xBF<p>\xC3\xA9</p>"), "<p>é</p>");
        assert_eq!(
            decode(b"<meta charset=\"iso-8859-1\"><p>\xE9</p>"),
            "<meta charset=\"iso-8859-1\"><p>é</p>"
        );
        assert_eq!(
            decode(b"<meta http-equiv=Content-Type content='text/html; charset=windows-1251'>\xC4"),
            "<meta http-equiv=Content-Type content='text/html; charset=windows-1251'>Д"
        );
        assert_eq!(decode(b"<p>\xE9</p>"), "<p>é</p>");
        assert_eq!(decode("<p>é</p>".as_bytes()), "<p>é</p>");
    }
}
