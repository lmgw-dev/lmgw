//! Which tool-result images go to OpenAI as images (openai egress design
//! §7.2.2): the Responses wire's `input_image` takes PNG, JPEG, WEBP and
//! non-animated GIF only (the vision guide's "Image input requirements",
//! checked 2026-10-25). Any other image, an SVG an MCP tool returned among
//! them, would make OpenAI refuse the whole send and every one after it in
//! the thread, so it becomes its placeholder, with a WARN, as on the Chat
//! wire.
//!
//! Decided by the declared mime *and* the bytes, with the llama.cpp
//! egress's readers (`egress::llama_cpp::tool_results`): both have to name
//! one of the four formats, and a GIF has to hold one image.

use crate::egress::llama_cpp::tool_results::{decode, format_name, gif_images, ImageFormat};

/// The format a tool-result image goes to OpenAI as, or why it does not.
pub(super) fn openai_image(mime: &str, data: &str) -> Result<ImageFormat, String> {
    let declared = ImageFormat::from_mime(mime)
        .filter(|f| {
            matches!(
                f,
                ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Webp | ImageFormat::Gif
            )
        })
        .ok_or_else(|| {
            format!(
                "{} is not an image format the Responses input takes (png, jpeg, webp or a still gif)",
                format_name(mime)
            )
        })?;
    let bytes = decode(data)
        .filter(|b| !b.is_empty())
        .ok_or("its data is not base64 image data")?;
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
    if declared == ImageFormat::Gif && gif_images(&bytes).is_some_and(|n| n > 1) {
        return Err("it is an animated gif, which the Responses input does not take".into());
    }
    Ok(declared)
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    use super::*;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// A GIF of `frames` 1x1 images.
    fn gif(frames: usize) -> Vec<u8> {
        let mut g = b"GIF89a".to_vec();
        g.extend([1, 0, 1, 0, 0x80, 0, 0]); // 1x1, a global table of 2
        g.extend([0, 0, 0, 255, 255, 255]);
        for _ in 0..frames {
            g.extend([0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0]);
            g.extend([2, 2, 0x44, 0x01, 0]);
        }
        g.push(0x3B);
        g
    }

    #[test]
    fn the_four_formats_go() {
        let png = b"\x89PNG\r\n\x1a\n";
        assert_eq!(openai_image("image/png", &b64(png)), Ok(ImageFormat::Png));
        assert_eq!(
            openai_image("image/JPG", &b64(&[0xFF, 0xD8, 0xFF, 0xE0])),
            Ok(ImageFormat::Jpeg)
        );
        let webp = b"RIFF\x04\0\0\0WEBPVP8 ";
        assert_eq!(
            openai_image("image/webp", &b64(webp)),
            Ok(ImageFormat::Webp)
        );
        assert_eq!(
            openai_image("image/gif", &b64(&gif(1))),
            Ok(ImageFormat::Gif)
        );
    }

    #[test]
    fn anything_else_is_its_placeholder_s_reason() {
        assert_eq!(
            openai_image("image/svg+xml", "PHN2Zy8+").unwrap_err(),
            "svg is not an image format the Responses input takes (png, jpeg, webp or a still gif)"
        );
        assert!(openai_image("image/bmp", &b64(b"BM")).is_err());
        assert_eq!(
            openai_image("image/gif", &b64(&gif(2))).unwrap_err(),
            "it is an animated gif, which the Responses input does not take"
        );
        assert_eq!(
            openai_image("image/png", &b64(&[0xFF, 0xD8, 0xFF])).unwrap_err(),
            "its bytes are jpeg, not the png it says it is"
        );
        assert!(openai_image("image/png", "").is_err());
    }
}
