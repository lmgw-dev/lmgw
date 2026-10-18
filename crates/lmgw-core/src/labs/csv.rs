//! CSV fields as RFC 4180 writes them, for the usage export and the labs'
//! flat tables.

/// `s` as one field: quoted, its quotes doubled, when it holds a comma, a
/// quote or a line break; as it is otherwise.
pub fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// One record: the fields joined by commas, ended by CRLF.
pub fn csv_line<S: AsRef<str>>(fields: &[S]) -> String {
    let mut out = fields
        .iter()
        .map(|f| csv_field(f.as_ref()))
        .collect::<Vec<_>>()
        .join(",");
    out.push_str("\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_is_quoted_only_when_it_must_be() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\"\nthen"), "\"say \"\"hi\"\"\nthen\"");
        assert_eq!(csv_line(&["a", "b,c", ""]), "a,\"b,c\",\r\n");
    }
}
