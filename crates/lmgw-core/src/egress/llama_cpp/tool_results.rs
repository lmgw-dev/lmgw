//! Tool-result images on llama.cpp (llama egress design §8): the renderer
//! that sends them, and the format check every one of them has to pass.
//!
//! llama-server takes `image_url` parts in any role, `tool` included, and
//! keeps the media marker where the part was; ik_llama.cpp's server does the
//! same (§6). So a tool result that carries an image the server can decode
//! goes as a `content` array, one part per block (§8.1). Whether a send may
//! do that at all is decided once per send (§8.2's predicate,
//! `gate::tool_images`), and arrives here as a [`ToolImageDecision`].
//!
//! **Nothing sendable, nothing changes.** A result without an image that
//! goes as an image is one string, built block by block by the shared
//! flattening ([`flatten_tool_result`]), so a result with no image at all is
//! today's string byte for byte. llama-server's own converter collapses a
//! text-only result the same way (`server-chat.cpp:499-505`).
//!
//! **The format check** ([`image_format`]) decides by the declared mime *and*
//! the bytes. llama-server takes a `data:` URL only with a `data:image/`
//! prefix and decodes whatever the bytes are (stb_image sniffs them), so the
//! declared mime alone would let a mislabelled image through to a decode
//! failure that fails the whole request. Both have to name the same format
//! llama.cpp decodes: png, jpeg, gif or bmp, and webp only where `/props`
//! says `video: true` (its decode needs a `MTMD_VIDEO` build,
//! `mtmd-helper.cpp:419-429`, `:509-516`). The data has to be plain base64,
//! which llama-server's decoder reads to its first other character, and is
//! decoded whole, once per check: the magic numbers say which format the
//! bytes are, and the file is then walked as stb_image reads it (`stb`),
//! so a truncated, 12-bit, arithmetic-coded or RLE image is its placeholder
//! too, never a request the server fails. A webp is only checked to be a
//! whole RIFF container holding a VP8, VP8L or VP8X chunk: ffmpeg decodes
//! it, not stb_image.

use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine as _;
use serde_json::{json, Value};

use crate::config::{LlamaRoute, ToolImages};
use crate::egress::openai_wire::ToolResultRenderer;
use crate::ir::{flatten_tool_result, tool_image_note, tool_image_placeholder, ToolResultBlock};

mod stb;
#[cfg(test)]
mod tests;

/// What one send may do with tool-result images: the renderer's whole input
/// (§8.2), made from the route's frozen decision and its facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolImageDecision<'a> {
    /// The predicate held. Every image that passes [`image_format`] goes as
    /// an `image_url` part; `webp` is whether the server decodes webp
    /// (`/props` said `video: true`).
    Send { webp: bool },
    /// It did not. Every image becomes its placeholder naming `reason`, with
    /// a WARN.
    Placeholder { reason: &'a str },
}

impl<'a> ToolImageDecision<'a> {
    /// The renderer's input from a route's frozen decision (§3.2): `None`
    /// when the decision is that nothing is known
    /// ([`ToolImages::Unknown`]), which renders today's bytes.
    pub fn of(route: &'a LlamaRoute) -> Option<Self> {
        match &route.tool_images {
            ToolImages::Allowed => Some(Self::Send {
                webp: route.facts.video == Some(true),
            }),
            ToolImages::Refused(reason) => Some(Self::Placeholder { reason }),
            ToolImages::Unknown => None,
        }
    }
}

/// The llama.cpp egress's tool-result rendering (§8.1), for a route whose
/// facts are known. A route with none keeps
/// [`FlattenToolResults`](crate::egress::openai_wire::FlattenToolResults):
/// unknown means today (decision 14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LlamaToolResults<'a>(pub ToolImageDecision<'a>);

/// One block of a tool result, as it is about to go out.
enum Part {
    Text(String),
    /// A `data:` URL.
    Image(String),
}

impl ToolResultRenderer for LlamaToolResults<'_> {
    fn render(&self, id: &str, content: &[ToolResultBlock]) -> Value {
        let mut parts = Vec::with_capacity(content.len());
        let mut dropped = Vec::new();
        for block in content {
            match block {
                ToolResultBlock::Image { mime, data } => match self.image(mime, data) {
                    Ok(format) => {
                        parts.push(Part::Image(format!("data:{};base64,{data}", format.mime())))
                    }
                    Err(why) => {
                        parts.push(Part::Text(tool_image_placeholder(mime, data, &why)));
                        dropped.push(format!("{} ({why})", tool_image_note(mime, data)));
                    }
                },
                // One block at a time through the shared flattening: each
                // block is one of its parts, so joined they are its string.
                other => {
                    let (text, notes) = flatten_tool_result(std::slice::from_ref(other));
                    parts.push(Part::Text(text));
                    dropped.extend(notes);
                }
            }
        }
        if !dropped.is_empty() {
            tracing::warn!(
                tool_call_id = %id,
                "tool result content not sent to this llama.cpp server as it is, replaced with \
                 placeholders: {}",
                dropped.join("; ")
            );
        }
        if !parts.iter().any(|p| matches!(p, Part::Image(_))) {
            let text: Vec<String> = parts
                .into_iter()
                .map(|p| match p {
                    Part::Text(t) | Part::Image(t) => t,
                })
                .collect();
            return Value::String(text.join("\n"));
        }
        Value::Array(
            parts
                .into_iter()
                .map(|p| match p {
                    Part::Text(text) => json!({"type": "text", "text": text}),
                    Part::Image(url) => json!({"type": "image_url", "image_url": {"url": url}}),
                })
                .collect(),
        )
    }
}

impl LlamaToolResults<'_> {
    /// Whether one image goes as an image on this send, and as what format.
    fn image(&self, mime: &str, data: &str) -> Result<ImageFormat, String> {
        match self.0 {
            ToolImageDecision::Send { webp } => image_format(mime, data, webp),
            ToolImageDecision::Placeholder { reason } => Err(reason.to_string()),
        }
    }
}

/// An image format llama.cpp decodes (§8.2): stb_image's png, jpeg, gif and
/// bmp, and webp through ffmpeg on a video build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Bmp,
    Webp,
}

impl ImageFormat {
    /// The mime its `data:` URL is sent with: always this lowercase one,
    /// since llama-server matches the `data:image/` prefix case-sensitively.
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Bmp => "image/bmp",
            Self::Webp => "image/webp",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Gif => "gif",
            Self::Bmp => "bmp",
            Self::Webp => "webp",
        }
    }

    /// The format a declared mime names, case and parameters aside.
    fn from_mime(mime: &str) -> Option<Self> {
        match essence(mime).as_str() {
            "image/png" => Some(Self::Png),
            "image/jpeg" | "image/jpg" => Some(Self::Jpeg),
            "image/gif" => Some(Self::Gif),
            "image/bmp" | "image/x-bmp" | "image/x-ms-bmp" => Some(Self::Bmp),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }

    /// The format the bytes are, by their magic numbers.
    fn sniff(head: &[u8]) -> Option<Self> {
        if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
            Some(Self::Png)
        } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if head.starts_with(b"BM") {
            Some(Self::Bmp)
        } else if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }
}

/// Why a webp image is not sent where `/props` did not say `video: true`.
pub const WEBP_NEEDS_VIDEO: &str =
    "webp is not a format this llama.cpp server decodes (its /props does not say video)";

/// The format check (§8.2, module doc): the format this image goes as, or why
/// it does not go. `webp` is whether the server decodes webp.
///
/// The one check both [`LlamaToolResults`] and `gate::tool_media` run, so
/// what is counted is what is sent.
pub fn image_format(mime: &str, data: &str, webp: bool) -> Result<ImageFormat, String> {
    let Some(declared) = ImageFormat::from_mime(mime) else {
        return Err(format!(
            "{} is not a format llama.cpp decodes",
            format_name(mime)
        ));
    };
    if declared == ImageFormat::Webp && !webp {
        return Err(WEBP_NEEDS_VIDEO.into());
    }
    if data.is_empty() {
        return Err("it carries no image data".into());
    }
    if !plain_base64(data) {
        return Err("its data is not plain base64".into());
    }
    let Some(bytes) = decode(data) else {
        return Err("its data is not plain base64".into());
    };
    match ImageFormat::sniff(&bytes) {
        Some(found) if found == declared => {}
        Some(found) => {
            return Err(format!(
                "its bytes are {}, not the {} it says it is",
                found.name(),
                declared.name()
            ))
        }
        None => return Err(format!("its bytes are not a {} image", declared.name())),
    }
    let whole = match declared {
        ImageFormat::Webp => webp_container(&bytes),
        stb_format => stb::check(stb_format, &bytes),
    };
    whole
        .map(|()| declared)
        .map_err(|why| format!("it is not a {} llama.cpp decodes ({why})", declared.name()))
}

/// A webp's RIFF container: its size covers a chunk, the data holds it, and
/// the first chunk is a bitstream (`VP8 `, lossless `VP8L`) or the extended
/// header (`VP8X`). The rest is ffmpeg's to decode.
fn webp_container(bytes: &[u8]) -> Result<(), String> {
    let size = bytes
        .get(4..8)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    match size {
        Some(size) if size >= 12 && bytes.len() >= 8 + size => {}
        _ => return Err("its RIFF container is cut off".into()),
    }
    match bytes.get(12..16) {
        Some(b"VP8 ") | Some(b"VP8L") | Some(b"VP8X") => Ok(()),
        _ => Err("its first chunk is not VP8, VP8L or VP8X".into()),
    }
}

/// A mime's essence: lowercase, without parameters.
fn essence(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// A format's name for a reason: an image mime's subtype without its `x-`,
/// `vnd.` or `+suffix` (`image/svg+xml` is "svg"), any other mime whole.
fn format_name(mime: &str) -> String {
    let essence = essence(mime);
    match essence.strip_prefix("image/") {
        Some(sub) => {
            let sub = sub.split('+').next().unwrap_or_default();
            let sub = sub.strip_prefix("x-").unwrap_or(sub);
            let sub = sub.strip_prefix("vnd.").unwrap_or(sub);
            sub.to_string()
        }
        None if essence.is_empty() => "an image without a mime type".into(),
        None => essence,
    }
}

/// What llama-server's decoder reads whole: the standard alphabet, and `=`
/// only as up to two characters of padding at the end.
fn plain_base64(data: &str) -> bool {
    let body = data.trim_end_matches('=');
    data.len() - body.len() <= 2
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// The whole image, padded or not; `None` when the base64 does not decode
/// (a length one past a whole group of four).
fn decode(data: &str) -> Option<Vec<u8>> {
    const LENIENT: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    LENIENT.decode(data).ok()
}
