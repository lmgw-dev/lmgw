//! E-mail messages (`.eml`, RFC 5322 with MIME; document-conversion design
//! §4.2, §5.2) through `mail-parser`: what a mail program shows of a
//! message, and its attachments as bytes, which the gateway reads through
//! its capability table like files of their own.
//!
//! **Which header fields.** Subject (as the heading), From, To, Cc and
//! Date: what a mail program shows above a message. The rest stay out: the
//! routing fields (`Received`, `Return-Path`, `Delivered-To`, which name
//! servers and addresses along the way), message ids, signatures (`DKIM`),
//! `Bcc` (the sender's own copy only) and every `X-` field.
//!
//! **The body.** The text body; an HTML body where the message has no text
//! one (through [`super::html`]: nothing is fetched, pictures are
//! placeholders). mail-parser decodes the transfer encoding and the
//! charset (`full_encoding`: every charset encoding_rs knows).
//!
//! **Attachments.** Each attachment's decoded bytes with its name; inline
//! pictures that the body shows are pictures, not attachments. A message
//! attached to a message is an attachment too. mail-parser's own bounds (a
//! part nested in at most 63 containers, at most 10,000 parts) are named
//! when a message reaches them. Decoding never makes a part larger than the
//! message it is in, so the attachments together are no larger than the
//! file.

use lmgw_doc_reader::wire::{Attachment, Failure, Mail, MAX_HTML_DEPTH};
use mail_parser::{AddressList, MessageParser, PartFlags};

use super::html;

fn addresses(a: Option<AddressList<'_>>) -> Option<String> {
    let list: Vec<String> = a?
        .mailboxes()
        .map(|m| match (m.name(), m.address()) {
            (Some(n), Some(e)) => format!("{n} <{e}>"),
            (Some(n), None) => n.to_string(),
            (None, Some(e)) => e.to_string(),
            (None, None) => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect();
    (!list.is_empty()).then(|| list.join(", "))
}

/// The message in `bytes` (module doc) and its attachments' bytes, in the
/// order of [`Mail::attachments`].
pub fn read(bytes: &[u8]) -> Result<(Mail, Vec<Vec<u8>>), Failure> {
    let msg = MessageParser::new().parse(bytes).ok_or_else(|| {
        Failure::Unreadable("the file is not a readable e-mail message".to_string())
    })?;
    let mut mail = Mail {
        subject: msg
            .subject()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        ..Default::default()
    };
    for (label, value) in [
        ("From", addresses(msg.from())),
        ("To", addresses(msg.to())),
        ("Cc", addresses(msg.cc())),
        ("Date", msg.date().map(|d| d.to_rfc3339())),
    ] {
        if let Some(v) = value {
            mail.fields.push(format!("{label}: {v}"));
        }
    }
    let texts: Vec<String> = msg
        .text_body()
        .filter(|p| p.is_content_type("text", "plain") || p.content_type().is_none())
        .filter_map(|p| p.text().map(|t| t.into_owned()))
        .collect();
    if !texts.is_empty() {
        mail.body = texts.join("\n\n");
        mail.plain = true;
    } else {
        let mut parts = Vec::new();
        let (mut deep, mut unparsed) = (0u32, 0u32);
        for h in msg.html_bodies() {
            match html::markdown(&h) {
                Ok(md) if md.too_deep.is_some() => deep += 1,
                Ok(md) => parts.push(md.text),
                Err(_) => unparsed += 1,
            }
        }
        if deep > 0 {
            mail.notes.push(format!(
                "the message's HTML body nests deeper than MAX_HTML_DEPTH ({MAX_HTML_DEPTH}) \
                 and was not read"
            ));
        }
        if unparsed > 0 {
            mail.notes
                .push("the message's HTML body could not be parsed".to_string());
        }
        mail.body = parts.join("\n\n");
    }
    let mut blobs = Vec::new();
    for (i, p) in msg.attachments().enumerate() {
        let count = i + 1;
        // A picture the body shows is part of the body.
        let ct = p.content_type();
        let image = ct
            .as_ref()
            .is_some_and(|c| c.ctype().eq_ignore_ascii_case("image"));
        if image && p.is_inline() {
            continue;
        }
        let content_type = ct
            .as_ref()
            .map(|c| match c.subtype() {
                Some(s) => format!("{}/{s}", c.ctype()),
                None => c.ctype().to_string(),
            })
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let name = p
            .attachment_name()
            .map(str::to_string)
            .unwrap_or_else(|| format!("attachment {count} ({content_type})"));
        let data = p.decoded().into_owned();
        mail.attachments.push(Attachment {
            name,
            content_type,
            len: data.len() as u64,
        });
        blobs.push(data);
    }
    let flagged = |f: PartFlags| msg.parts().any(|p| p.flags().contains(f));
    if flagged(PartFlags::NESTING_LIMIT) {
        mail.notes.push(
            "parts nested in more than 63 containers (mail-parser's bound) were not read"
                .to_string(),
        );
    }
    if flagged(PartFlags::LIMIT_REACHED) {
        mail.notes.push(
            "the message has more than 10,000 parts (mail-parser's bound); the rest was not \
             read"
                .to_string(),
        );
    }
    Ok((mail, blobs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIL: &str = "Received: from relay.example (192.0.2.7) by mx.example\r\n\
        From: Ann Example <ann@example.com>\r\n\
        To: Bob Example <bob@example.com>, team@example.com\r\n\
        Bcc: hidden@example.com\r\n\
        Subject: =?UTF-8?Q?Gr=C3=BC=C3=9Fe?=\r\n\
        Date: Tue, 17 Nov 2026 10:00:00 +0100\r\n\
        Message-ID: <id@example.com>\r\n\
        X-Originating-IP: 198.51.100.4\r\n\
        MIME-Version: 1.0\r\n\
        Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
        --b\r\nContent-Type: text/plain; charset=iso-8859-1\r\n\
        Content-Transfer-Encoding: quoted-printable\r\n\r\n\
        Hallo B=F6b,\r\n\r\nsiehe Anhang.\r\n\
        --b\r\nContent-Type: text/plain; name=\"notes.txt\"\r\n\
        Content-Disposition: attachment; filename=\"notes.txt\"\r\n\
        Content-Transfer-Encoding: base64\r\n\r\n\
        SGVsbG8gZnJvbSB0aGUgYXR0YWNobWVudA==\r\n\
        --b--\r\n";

    #[test]
    fn a_message_reads_as_a_mail_program_shows_it() {
        let (m, blobs) = read(MAIL.as_bytes()).unwrap();
        assert_eq!(m.subject.as_deref(), Some("Grüße"));
        assert_eq!(
            m.fields,
            [
                "From: Ann Example <ann@example.com>",
                "To: Bob Example <bob@example.com>, team@example.com",
                "Date: 2026-11-17T10:00:00+01:00",
            ]
        );
        assert!(m.plain);
        assert!(m.body.contains("Hallo Böb"), "{}", m.body);
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].name, "notes.txt");
        assert_eq!(m.attachments[0].len, 25);
        assert_eq!(blobs, [b"Hello from the attachment".to_vec()]);
        let all = format!("{:?}", m);
        for gone in ["192.0.2.7", "hidden@", "id@example.com", "198.51.100.4"] {
            assert!(!all.contains(gone), "{gone}");
        }
    }

    #[test]
    fn an_html_only_message_is_converted_without_fetching() {
        let raw = "From: a@example.com\r\nTo: b@example.com\r\nSubject: x\r\n\
                   Content-Type: text/html; charset=utf-8\r\n\r\n\
                   <h1>Hi</h1><p>See <img src=\"https://tracker.example/p.gif\"></p>";
        let (m, _) = read(raw.as_bytes()).unwrap();
        assert!(!m.plain);
        assert!(m.body.starts_with("# Hi"), "{}", m.body);
        assert!(!m.body.contains("tracker"), "{}", m.body);
        let deep = format!(
            "From: a@example.com\r\nTo: b@example.com\r\nSubject: x\r\n\
             Content-Type: text/html\r\n\r\n{}x",
            "<div>".repeat(600)
        );
        let (m, _) = read(deep.as_bytes()).unwrap();
        assert!(m.body.is_empty());
        assert!(m.notes[0].contains("MAX_HTML_DEPTH"), "{:?}", m.notes);
    }
}
