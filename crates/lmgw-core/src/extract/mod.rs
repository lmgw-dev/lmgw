//! Text out of uploaded files, shared by Chat attachments (chat-complete
//! design §8) and knowledge-base ingestion (§9.2): what a file is
//! ([`sniff`]), its text as pages ([`pdf`]) or markdown ([`office`]), and the
//! vision prompts for pages that have none ([`vision_prompts`]).
//!
//! Nothing here touches the store, the gateway state or the chat: bytes in,
//! text out, every failure a visible error. [`extract`] is the one-call
//! convenience; a caller that needs more (PDF page classification, page
//! images, per-sheet parts) uses the submodules directly.

pub mod office;
mod office_ods;
mod office_sheet;
mod office_xlsx;
mod office_xlsx_dates;
pub mod pdf;
pub mod sniff;
pub mod vision_prompts;

#[cfg(test)]
pub(crate) mod test_files;

use bytes::Bytes;

pub use office::{OfficeError, OfficeFormat, OfficeText};
pub use pdf::{PdfClass, PdfError, PdfText};
pub use sniff::{sniff, sniff_async, Kind, SniffError, Sniffed, ACCEPTED};

/// The text of a file, in the shape its kind gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extracted {
    /// A UTF-8 text file, as is.
    Text(String),
    /// A PDF: its text per page, and which pages have none.
    Pdf(PdfText),
    /// An office file: markdown parts (a document, slides, sheets).
    Office(OfficeText),
}

impl Extracted {
    /// One text for the whole file: text as is, a PDF's pages under
    /// `--- page N ---` markers (text-less pages left out —
    /// [`PdfText::marked`]), an office file as [`OfficeText::markdown`].
    pub fn text(&self) -> String {
        match self {
            Self::Text(t) => t.clone(),
            Self::Pdf(p) => p.marked(),
            Self::Office(o) => o.markdown(),
        }
    }
}

/// Why nothing could be extracted.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("{0} files have no text to extract")]
    NoText(&'static str),
    #[error("the text file is not valid UTF-8")]
    NotUtf8,
    #[error(transparent)]
    Pdf(#[from] PdfError),
    #[error(transparent)]
    Office(#[from] OfficeError),
    #[error("reading the file was interrupted: {0}")]
    Interrupted(String),
}

/// Extract the text of `bytes`, which [`sniff`] called `what`. Images and
/// audio have none ([`ExtractError::NoText`]). Office parsing runs in
/// `spawn_blocking`; PDFs go through `pdftotext` (needs poppler-utils).
pub async fn extract(what: &Sniffed, bytes: Bytes) -> Result<Extracted, ExtractError> {
    match what.kind {
        Kind::Text => String::from_utf8(bytes.to_vec())
            .map(Extracted::Text)
            .map_err(|_| ExtractError::NotUtf8),
        Kind::Pdf => Ok(Extracted::Pdf(pdf::text(bytes).await?)),
        Kind::Office => {
            let format = what
                .office_format()
                .ok_or(ExtractError::NoText("unrecognised office"))?;
            let text = tokio::task::spawn_blocking(move || office::extract(format, &bytes))
                .await
                .map_err(|e| ExtractError::Interrupted(e.to_string()))??;
            Ok(Extracted::Office(text))
        }
        Kind::Image => Err(ExtractError::NoText("image")),
        Kind::Audio => Err(ExtractError::NoText("audio")),
    }
}

/// The token estimate the chips show: characters over four (the `~` marks it
/// as an estimate — a tokenizer would need the model).
pub fn approx_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn extract_dispatches_on_the_sniffed_kind() {
        let doc = test_files::docx(&["Hello", "World"]);
        let s = sniff(&doc).unwrap();
        let out = extract(&s, Bytes::from(doc)).await.unwrap();
        assert_eq!(out.text(), "Hello\n\nWorld");

        let text = b"plain \xc3\xa9".to_vec();
        let s = sniff(&text).unwrap();
        assert_eq!(
            extract(&s, Bytes::from(text)).await.unwrap(),
            Extracted::Text("plain é".into())
        );

        let png = b"\x89PNG\r\n\x1a\n".to_vec();
        let s = sniff(&png).unwrap();
        assert!(matches!(
            extract(&s, Bytes::from(png)).await,
            Err(ExtractError::NoText("image"))
        ));
        let wav = b"RIFF\0\0\0\0WAVE".to_vec();
        let s = sniff(&wav).unwrap();
        assert!(matches!(
            extract(&s, Bytes::from(wav)).await,
            Err(ExtractError::NoText("audio"))
        ));
    }

    #[test]
    fn the_token_estimate_is_chars_over_four_rounded_up() {
        assert_eq!(approx_tokens(""), 0);
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("abcde"), 2);
        assert_eq!(approx_tokens("éééé"), 1);
    }
}
