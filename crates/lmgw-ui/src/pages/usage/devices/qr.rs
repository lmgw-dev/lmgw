//! The pairing link as a QR code (device link design §3.1): the same string
//! the card shows as text, drawn as an SVG by the `qrcode` crate (pure Rust,
//! no image decoder). Dark modules on a white quiet zone whatever the
//! dashboard's palette, since a phone's camera reads that best.

use qrcode::render::svg;
use qrcode::{EcLevel, QrCode};

/// `link` as an SVG document, at least `px` wide and tall. The error is the
/// encoder's own, in a sentence: a link too long for any QR code gets the
/// link alone (§3.1).
pub(super) fn svg_of(link: &str, px: u32) -> Result<String, String> {
    // M corrects 15% of the modules, enough for a screen; a lower level
    // fits a few more bytes, which the longest link a URL list can make
    // would need before the encoder gives up.
    let code = QrCode::with_error_correction_level(link.as_bytes(), EcLevel::M)
        .or_else(|_| QrCode::with_error_correction_level(link.as_bytes(), EcLevel::L))
        .map_err(|e| format!("the link is too long for a QR code ({e})"))?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(px, px)
        .quiet_zone(true)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str = "lmgw-pair:?v=2&name=phone&url=https%3A%2F%2F192.0.2.1%3A8788&url=https%3A%2F%2F198.51.100.20%3A8788&fp=sha256%3A9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c&token=lmgw-enrol-4b1d4b1d4b1d4b1d4b1d4b1d4b1d4b1d";

    #[test]
    fn a_pairing_link_is_drawn_as_an_svg() {
        let svg = svg_of(LINK, 220).unwrap();
        assert!(
            svg.starts_with("<?xml") || svg.starts_with("<svg"),
            "{svg:.60}"
        );
        assert!(svg.contains("<svg") && svg.contains("#ffffff") && svg.contains("#000000"));
        // The link is data in the modules, not text in the document: the
        // token is not in what the page prints.
        assert!(!svg.contains("lmgw-enrol"));
    }

    #[test]
    fn a_link_past_every_qr_code_is_refused_with_a_sentence() {
        let long = format!("{LINK}&url={}", "a".repeat(4000));
        let e = svg_of(&long, 220).unwrap_err();
        assert!(e.contains("too long for a QR code"), "{e}");
    }
}
