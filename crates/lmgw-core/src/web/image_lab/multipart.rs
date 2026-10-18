//! The one `multipart/form-data` body an edit is relayed as: the form's text
//! fields in the order the form lists them, then the picture and the mask,
//! as `/v1/images/edits` reads them.

use axum::body::Bytes;

/// One file part: its field name, file name, declared type and bytes.
#[derive(Debug, Clone)]
pub(super) struct FilePart {
    pub field: String,
    pub filename: String,
    pub content_type: Option<String>,
    pub bytes: Bytes,
}

/// `name` as it can sit in a quoted `filename="…"`: without the characters
/// that end the quoted string early (`"`, and a trailing `\` escaping the
/// closing quote) or the header line (CR, LF, any control character).
/// Dropped rather than escaped, because the name is a label here — the
/// bytes are the upload.
fn header_safe(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '"' | '\\') && !c.is_control())
        .collect()
}

/// Serialize one `multipart/form-data` body: the text fields in the order the
/// form lists them, then the file parts.
///
/// Hand-written because the destination is an in-process [`axum::extract::Request`]
/// rather than an outgoing reqwest call — `reqwest::multipart::Form` can only
/// be handed to a `RequestBuilder`, and `proxy::handle_image_edit` takes the
/// request a client would have sent.
pub(super) fn multipart_body(
    boundary: &str,
    fields: &[(String, String)],
    files: &[FilePart],
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        out.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    for f in files {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        out.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                f.field,
                header_safe(&f.filename)
            )
            .as_bytes(),
        );
        let ct = f
            .content_type
            .as_deref()
            .map(header_safe)
            .unwrap_or_else(|| "application/octet-stream".into());
        out.extend_from_slice(format!("Content-Type: {ct}\r\n\r\n").as_bytes());
        out.extend_from_slice(&f.bytes);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(filename: &str, content_type: Option<&str>) -> FilePart {
        FilePart {
            field: "image".into(),
            filename: filename.into(),
            content_type: content_type.map(String::from),
            bytes: Bytes::from_static(b"\x89PNGfake"),
        }
    }

    #[test]
    fn the_multipart_carries_every_field_and_file() {
        let fields = vec![
            ("model".to_string(), "image/z".to_string()),
            ("prompt".to_string(), "make it night".to_string()),
        ];
        let files = vec![part("in.png", Some("image/png"))];
        let body = multipart_body("BOUND", &fields, &files);
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("--BOUND\r\n"));
        assert!(text.contains("name=\"model\"\r\n\r\nimage/z\r\n"));
        assert!(text.contains("name=\"prompt\"\r\n\r\nmake it night\r\n"));
        assert!(text.contains("name=\"image\"; filename=\"in.png\""));
        assert!(text.contains("Content-Type: image/png"));
        assert!(text.ends_with("--BOUND--\r\n"));
    }

    /// Neither a quote, a backslash nor a line break in a file name can
    /// close the header and inject a part or a header of its own — a
    /// trailing `\` would escape the quote that ends `filename="…"`, and a
    /// CR LF would start a header line.
    #[test]
    fn a_quoted_escaped_or_broken_file_name_cannot_break_out_of_its_header() {
        let text = |name: &str| {
            String::from_utf8_lossy(&multipart_body("BOUND", &[], &[part(name, None)])).to_string()
        };
        let t = text("a\"; name=\"model");
        assert!(t.contains("filename=\"a; name=model\""));
        assert_eq!(t.matches("name=\"image\"").count(), 1);

        let t = text("shot\\");
        assert!(t.contains("filename=\"shot\"\r\n"), "{t}");
        assert!(!t.contains('\\'), "{t}");

        let t = text("a.png\r\nContent-Type: text/html\r\n\r\n<b>");
        assert!(
            t.contains("filename=\"a.pngContent-Type: text/html<b>\"\r\n"),
            "{t}"
        );
        assert!(
            !t.contains("\r\nContent-Type: text/html"),
            "no header of its own: {t}"
        );
    }
}
