//! Which pictures a model can be sent (design §5.3, §9): a PNG or a JPEG,
//! the two formats every model lmgw routes to takes (llama.cpp's
//! projectors, OpenAI, Anthropic, Gemini), of at most
//! [`PAGE_MAX_PIXELS`](crate::extract::convert::model_help::render::PAGE_MAX_PIXELS),
//! the page-reading model's own bound on an image. Anything else is not
//! sent yet, with a `not_yet` note: a GIF, WebP, BMP or TIFF would have to
//! be re-encoded, and a larger picture scaled, and both mean decoding an
//! untrusted picture, which belongs in lmgw's document reader (a process of
//! its own), not in the gateway; the reader has no such step yet.

use bytes::Bytes;
use lmgw_api_types::documents::convert::{ConvertWarning, WarningCode};

use crate::extract::convert::doc::Source;
use crate::extract::convert::model_help::render::PAGE_MAX_PIXELS;
use crate::extract::convert::pictures::{self, Image, Picture};

/// Why a picture is not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum NotSent {
    /// Its format is not one every model takes: its media type.
    Format(&'static str),
    /// It has more pixels than [`PAGE_MAX_PIXELS`].
    Pixels,
    /// Its size could not be read from its header.
    Size,
}

impl NotSent {
    /// Why, in a note's words.
    pub(super) fn said(self) -> String {
        match self {
            Self::Format(mime) => format!(
                "a picture stored as {mime} is not sent to a model yet: models take PNG and \
                 JPEG, and lmgw does not convert pictures for them yet"
            ),
            Self::Pixels => format!(
                "a picture of more than PAGE_MAX_PIXELS ({:.1} million pixels) is not sent to a \
                 model yet: lmgw does not scale pictures down for them yet",
                PAGE_MAX_PIXELS as f64 / 1e6
            ),
            Self::Size => "a picture whose size lmgw cannot read from its header is not sent to \
                           a model"
                .to_string(),
        }
    }
}

/// The type `img` is sent as, or why it is not sent.
pub(super) fn check(img: &Image) -> Result<&'static str, NotSent> {
    if !matches!(img.mime, "image/png" | "image/jpeg") {
        return Err(NotSent::Format(img.mime));
    }
    let (Some(w), Some(h)) = (img.width_px, img.height_px) else {
        return Err(NotSent::Size);
    };
    if u64::from(w).saturating_mul(u64::from(h)) > PAGE_MAX_PIXELS {
        return Err(NotSent::Pixels);
    }
    Ok(img.mime)
}

/// An image file as the one picture of its first part, its bytes as
/// stored (whether a model can be sent it is the description's own check,
/// [`check`]); when lmgw cannot tell its type, the note that says it is not
/// exported.
pub(crate) fn image_file(src: &Bytes) -> Result<Picture, ConvertWarning> {
    // SVG carries scripts and outside links: never exported (WP10).
    let kind = pictures::sniff::kind(src).filter(|k| k.mime != "image/svg+xml");
    let Some(kind) = kind else {
        return Err(ConvertWarning {
            code: WarningCode::PicturesPartial,
            message: "the image file is not exported as a picture: lmgw cannot tell its type \
                      from its bytes, or it is SVG"
                .to_string(),
            tool: None,
            parts: Vec::new(),
            option: Some("pictures".to_string()),
        });
    };
    let size = pictures::sniff::size(src);
    let mut p = Picture::new(1, None, Source::Native);
    p.image = Some(Image {
        bytes: src.clone(),
        mime: kind.mime,
        ext: kind.ext,
        width_px: size.map(|s| s.0),
        height_px: size.map(|s| s.1),
    });
    // The whole image is the picture: all of its part.
    p.share = Some(1.0);
    Ok(p)
}
