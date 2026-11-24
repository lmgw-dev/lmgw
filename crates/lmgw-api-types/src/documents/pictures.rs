//! A converted file's pictures (document-conversion design §5.3, §7.1): the
//! `pictures` list of each file's entry, in reading order. Each `figure`
//! block of `output: json` and each picture line of the markdown names its
//! picture by `id`.
//!
//! Whether a picture carries its bytes follows the request's `pictures`:
//! - `placeholder`: no bytes (`file` and `data_base64` are `null`);
//! - `embedded`: the markdown holds the picture as a `data:` URI, so the
//!   list does not repeat it; with `output: json` there is no markdown and
//!   the bytes are here;
//! - `referenced`: the markdown links `file`, and the bytes are here.

use serde::{Deserialize, Serialize};

/// One exported picture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocPicture {
    /// `p1`, `p2`, … in reading order within the file.
    pub id: String,
    /// The part it is on: a PDF page's number, a slide's position; 1 for a
    /// document without pages.
    pub part: u32,
    /// `[x0, y0, x1, y1]` in points from the page's top-left corner; `null`
    /// where the part has no geometry (office files).
    pub bbox: Option<[f64; 4]>,
    /// The caption beside it (a line starting `Figure`, `Fig.`, `Abbildung`
    /// … right below or above it), when there is one.
    pub caption: Option<String>,
    /// The media type of the bytes: `image/png` for a PDF picture (cropped
    /// from the rendered page), an office file's own (`image/jpeg`, …).
    pub mime: Option<String>,
    /// Its size in pixels, where lmgw knows it.
    pub width_px: Option<u32>,
    pub height_px: Option<u32>,
    /// The size of its bytes, where lmgw has them.
    pub bytes: Option<u64>,
    /// The relative name the markdown links (`referenced`) and the docling
    /// routes' zip uses: `artifacts/image_<n:06>_<sha256>.<ext>`, `n`
    /// counting the file's pictures from 0 and the hash taken over the
    /// bytes. `null` when the bytes are not in this entry.
    pub file: Option<String>,
    /// The bytes, base64 (standard alphabet, padded).
    pub data_base64: Option<String>,
    /// What the picture-description model said of it, when the request
    /// asked for descriptions (`describe_pictures`) and it was described;
    /// `null` otherwise.
    pub description: Option<String>,
}
