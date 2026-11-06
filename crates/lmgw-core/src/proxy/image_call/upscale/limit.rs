//! The largest picture an upscale may make, checked before anything is
//! admitted (chat-media-tools design §3.3, §14): the input's size times the
//! upscaler's scale, once per repeat, against the chosen row's probed
//! `limits.max_upscale_width` / `max_upscale_height` — sd-server's own check
//! (`valid_upscale_dimensions`, every run's size within the limit), said
//! with the numbers before a model is loaded for it.
//!
//! What it cannot know it does not guess: a row never probed, a picture
//! whose header it cannot read (PNG, JPEG, WebP and GIF are read), or an
//! upscaler whose scale neither the shipped list nor its name says, leaves
//! the check to the server, whose refusal reaches the caller as its error.

use crate::error::GatewayError;
use crate::runtime::image::{ImageCapabilities, ImageLimits};
use crate::state::SharedState;

use super::UpscaleCall;

/// What one upscale asks for.
#[derive(Debug, Clone, Copy)]
pub(super) struct Asked {
    /// The input picture's size.
    pub size: (u32, u32),
    /// The upscaler's scale per run.
    pub scale: u32,
    pub repeats: u32,
}

/// The refusal when `asked` makes a picture larger than `limits` allow on
/// row `alias`; `None` when it fits or the row names no limit.
pub(super) fn refusal(alias: &str, limits: &ImageLimits, asked: Asked) -> Option<GatewayError> {
    let (max_w, max_h) = limits.max_upscale_width.zip(limits.max_upscale_height)?;
    let (mut w, mut h) = (u64::from(asked.size.0), u64::from(asked.size.1));
    let scale = u64::from(asked.scale.max(1));
    for _ in 0..asked.repeats.max(1) {
        w *= scale;
        h *= scale;
    }
    if w <= max_w.max(0) as u64 && h <= max_h.max(0) as u64 {
        return None;
    }
    let (iw, ih) = asked.size;
    let times = match asked.repeats {
        0 | 1 => format!("{}x", asked.scale),
        r => format!("{}x {r} times", asked.scale),
    };
    Some(GatewayError::Refused {
        status: 422,
        code: "too_large",
        message: format!(
            "upscaling this {iw}x{ih} picture {times} makes {w}x{h}, and {alias}'s image \
             server makes at most {max_w}x{max_h} (its max_upscale_width and \
             max_upscale_height): use fewer repeats, an upscaler of a smaller scale, or a \
             smaller picture"
        ),
    })
}

/// The refusal when `call` would make a picture larger than `caps`
/// allow (module doc); `None` when it fits or cannot be known.
pub(super) fn too_large(
    state: &SharedState,
    alias: &str,
    caps: &ImageCapabilities,
    call: &UpscaleCall<'_>,
) -> Option<GatewayError> {
    let size = dims(&call.image.bytes)?;
    let scale = match call.upscaler {
        Some(u) => crate::image_upscalers::scale_of(u)?,
        // The server takes its first compatible one: the smallest scale
        // on disk is the least it makes, when every one says its scale.
        None => {
            let dir = state.snapshot().settings.image.models_dir.clone();
            crate::image_upscalers::names(&dir)
                .iter()
                .map(|n| crate::image_upscalers::scale_of(n))
                .collect::<Option<Vec<u32>>>()?
                .into_iter()
                .min()?
        }
    };
    let asked = Asked {
        size,
        scale,
        repeats: call.repeats.unwrap_or(1),
    };
    refusal(alias, &caps.limits, asked)
}

/// `(width, height)` from a PNG, JPEG, WebP or GIF header; `None` for
/// anything else or a header cut short.
pub(super) fn dims(bytes: &[u8]) -> Option<(u32, u32)> {
    super::super::body::png_size(bytes)
        .or_else(|| jpeg(bytes))
        .or_else(|| webp(bytes))
        .or_else(|| gif(bytes))
}

/// A JPEG's size from its first start-of-frame marker.
fn jpeg(b: &[u8]) -> Option<(u32, u32)> {
    if !b.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let marker = b[i + 1];
        // Fill bytes, and markers without a length.
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([b[i + 2], b[i + 3]]));
        let sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if sof {
            let s = b.get(i + 5..i + 9)?;
            let h = u16::from_be_bytes([s[0], s[1]]);
            let w = u16::from_be_bytes([s[2], s[3]]);
            return Some((u32::from(w), u32::from(h)));
        }
        i += 2 + len;
    }
    None
}

/// A WebP's size: lossy (`VP8 `), lossless (`VP8L`) or extended (`VP8X`).
fn webp(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 30 || &b[..4] != b"RIFF" || &b[8..12] != b"WEBP" {
        return None;
    }
    let le24 = |s: &[u8]| u32::from(s[0]) | u32::from(s[1]) << 8 | u32::from(s[2]) << 16;
    match &b[12..16] {
        b"VP8 " => {
            let w = u16::from_le_bytes([b[26], b[27]]) & 0x3FFF;
            let h = u16::from_le_bytes([b[28], b[29]]) & 0x3FFF;
            Some((u32::from(w), u32::from(h)))
        }
        b"VP8L" => {
            let v = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
            Some(((v & 0x3FFF) + 1, ((v >> 14) & 0x3FFF) + 1))
        }
        b"VP8X" => Some((le24(&b[24..27]) + 1, le24(&b[27..30]) + 1)),
        _ => None,
    }
}

/// A GIF's logical screen size.
fn gif(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 10 || !(b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return None;
    }
    let w = u16::from_le_bytes([b[6], b[7]]);
    let h = u16::from_le_bytes([b[8], b[9]]);
    Some((u32::from(w), u32::from(h)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(w: i64, h: i64) -> ImageLimits {
        ImageLimits {
            max_upscale_width: Some(w),
            max_upscale_height: Some(h),
            ..Default::default()
        }
    }

    #[test]
    fn a_picture_within_the_limit_passes_and_one_over_it_is_said_with_the_numbers() {
        let fits = Asked {
            size: (2048, 1024),
            scale: 4,
            repeats: 1,
        };
        assert!(refusal("image/z", &limits(8192, 8192), fits).is_none());
        let twice = Asked { repeats: 2, ..fits };
        let e = refusal("image/z", &limits(8192, 8192), twice).unwrap();
        let GatewayError::Refused {
            status,
            code,
            message,
        } = e
        else {
            panic!("{e:?}");
        };
        assert_eq!((status, code), (422, "too_large"));
        assert!(
            message.starts_with(
                "upscaling this 2048x1024 picture 4x 2 times makes 32768x16384, and \
                 image/z's image server makes at most 8192x8192"
            ),
            "{message}"
        );
        // No limit named: nothing to check against.
        assert!(refusal("image/z", &ImageLimits::default(), twice).is_none());
    }

    #[test]
    fn sizes_are_read_from_every_header_the_chat_takes() {
        assert_eq!(dims(&crate::bench::png::solid_red()), Some((64, 64)));
        // A baseline JPEG: SOI, an APP0 segment, SOF0 with 480 high, 640 wide.
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        jpg.extend([0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0xE0, 0x02, 0x80]);
        assert_eq!(dims(&jpg), Some((640, 480)));
        let mut gif = b"GIF89a".to_vec();
        gif.extend([0x20, 0x03, 0x58, 0x02]);
        assert_eq!(dims(&gif), Some((800, 600)));
        let mut vp8x = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        vp8x.extend([0u8; 8]);
        vp8x.extend([0xFF, 0x03, 0x00, 0xFF, 0x01, 0x00]);
        assert_eq!(dims(&vp8x), Some((1024, 512)));
        assert_eq!(dims(b"not a picture"), None);
    }
}
