//! Whole images of every kind stb_image decodes pass; each failure the
//! module doc names fails with its reason. The images are small whole files
//! written by Pillow, mutated byte by byte.

use base64::Engine as _;

use super::check;
use crate::egress::llama_cpp::tool_results::ImageFormat::{self, Bmp, Gif, Jpeg, Png};

/// A whole 1x1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
/// A whole 8x8 grey baseline JPEG (SOF0).
const JPEG: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDABALDA4MChAODQ4SERATGCgaGBYWGDEjJR0oOjM9PDkzODdASFxOQERXRTc4UG1RV19iZ2hnPk1xeXBkeFxlZ2P/wAALCAAIAAgBAREA/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/9oACAEBAAA/ACv/2Q==";
/// A whole 8x8 grey progressive JPEG (SOF2).
const JPEG_PROGRESSIVE: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDABALDA4MChAODQ4SERATGCgaGBYWGDEjJR0oOjM9PDkzODdASFxOQERXRTc4UG1RV19iZ2hnPk1xeXBkeFxlZ2P/wgALCAAIAAgBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAAAP/aAAgBAQAAAAF//8QAFBABAAAAAAAAAAAAAAAAAAAAAP/aAAgBAQABBQJ//8QAFBABAAAAAAAAAAAAAAAAAAAAAP/aAAgBAQAGPwJ//8QAFBABAAAAAAAAAAAAAAAAAAAAAP/aAAgBAQABPyF//9oACAEBAAAAEH//xAAUEAEAAAAAAAAAAAAAAAAAAAAA/9oACAEBAAE/EH//2Q==";
/// A whole 1x1 GIF.
const GIF: &str = "R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw==";
/// A whole 2x2 24-bit BMP (40-byte header).
const BMP: &str = "Qk1GAAAAAAAAADYAAAAoAAAAAgAAAAIAAAABABgAAAAAABAAAADEDgAAxA4AAAAAAAAAAAAAKB7IKB7IAAAoHsgoHsgAAA==";
/// A whole 3x2 1-bit BMP with its two-entry palette.
const BMP_1BIT: &str = "Qk1GAAAAAAAAAD4AAAAoAAAAAwAAAAIAAAABAAEAAAAAAAgAAADEDgAAxA4AAAIAAAACAAAAAAAAAP///wDgAAAA4AAAAA==";

fn bytes(b64: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("a test image")
}

/// The reason `format`'s check gives for `data`.
fn why(format: ImageFormat, data: &[u8]) -> String {
    check(format, data).expect_err("the check fails")
}

/// Where `marker` (`FF xx`) first is in a JPEG.
fn at_marker(jpeg: &[u8], marker: u8) -> usize {
    jpeg.windows(2)
        .position(|w| w == [0xFF, marker])
        .expect("the marker is there")
}

#[test]
fn whole_images_of_every_kind_pass() {
    for (format, data) in [
        (Png, PNG),
        (Jpeg, JPEG),
        (Jpeg, JPEG_PROGRESSIVE),
        (Gif, GIF),
        (Bmp, BMP),
        (Bmp, BMP_1BIT),
    ] {
        assert_eq!(check(format, &bytes(data)), Ok(()), "{format:?}");
    }
}

#[test]
fn a_png_has_to_be_whole() {
    let png = bytes(PNG);
    assert_eq!(why(Png, &png[..8]), "it ends before its IHDR chunk");
    // Cut inside the IDAT, and right before the IEND.
    assert_eq!(why(Png, &png[..45]), "it ends inside its IDAT chunk");
    let iend = png.len() - 12;
    assert_eq!(why(Png, &png[..iend]), "it ends before its IEND chunk");
    // Bytes after the IEND are never read.
    let mut trailing = png.clone();
    trailing.extend_from_slice(b"anything");
    assert_eq!(check(Png, &trailing), Ok(()));
}

#[test]
fn a_png_fails_where_stb_image_fails() {
    let png = bytes(PNG);
    // IHDR: width at 16, bit depth at 24, colour type at 25.
    let mut zero = png.clone();
    zero[16..20].copy_from_slice(&[0; 4]);
    assert_eq!(why(Png, &zero), "it has no pixels");
    let mut depth = png.clone();
    depth[24] = 3;
    assert_eq!(why(Png, &depth), "its bit depth is 3");
    let mut colour = png.clone();
    colour[25] = 5;
    assert_eq!(
        why(Png, &colour),
        "its colour type 5 at depth 8 is not one PNG has"
    );
    // A critical chunk stb_image does not know, in place of the IDAT.
    let mut critical = png.clone();
    critical[37..41].copy_from_slice(b"ABCD");
    assert_eq!(
        why(Png, &critical),
        "it has a critical chunk ABCD stb_image does not know"
    );
    // An ancillary one is skipped, but then there is no IDAT.
    let mut ancillary = png.clone();
    ancillary[37..41].copy_from_slice(b"abCD");
    assert_eq!(why(Png, &ancillary), "it has no IDAT chunk");
    // The IDAT's zlib header: compression method 8 (deflate) only.
    let mut zlib = png.clone();
    zlib[41] = 0x79;
    assert_eq!(
        why(Png, &zlib),
        "its IDAT data does not start with a zlib header"
    );
}

#[test]
fn a_jpeg_needs_an_8_bit_frame_stb_image_decodes() {
    let jpeg = bytes(JPEG);
    let sof = at_marker(&jpeg, 0xC0);
    let patched = |marker: u8, precision: u8| {
        let mut j = jpeg.clone();
        j[sof + 1] = marker;
        j[sof + 4] = precision;
        j
    };
    // Extended sequential at 8 bits is baseline to stb_image.
    assert_eq!(check(Jpeg, &patched(0xC1, 8)), Ok(()));
    let twelve = "its samples are 12-bit, and stb_image decodes 8-bit only";
    assert_eq!(why(Jpeg, &patched(0xC1, 12)), twelve);
    assert_eq!(why(Jpeg, &patched(0xC0, 12)), twelve);
    assert_eq!(why(Jpeg, &patched(0xC3, 8)), "it is a lossless JPEG (SOF3)");
    assert_eq!(
        why(Jpeg, &patched(0xC5, 8)),
        "it is a differential JPEG (SOF5)"
    );
    for sof in [0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF] {
        assert_eq!(
            why(Jpeg, &patched(sof, 8)),
            format!("it is an arithmetic-coded JPEG (SOF{})", sof - 0xC0)
        );
    }
    assert_eq!(
        why(Jpeg, &patched(0xF7, 8)),
        "it is a JPEG-LS image (SOF55)"
    );
}

#[test]
fn a_jpeg_has_to_reach_its_eoi() {
    let jpeg = bytes(JPEG);
    let n = jpeg.len();
    assert_eq!(
        why(Jpeg, &jpeg[..n - 2]),
        "it ends inside a scan, before its EOI marker"
    );
    assert_eq!(
        why(Jpeg, &jpeg[..at_marker(&jpeg, 0xC0)]),
        "it has no frame (SOF) marker"
    );
    assert_eq!(why(Jpeg, &jpeg[..30]), "it ends inside its DQT segment");
    // A frame and no scan.
    let mut scanless = jpeg[..at_marker(&jpeg, 0xDA)].to_vec();
    scanless.extend_from_slice(&[0xFF, 0xD9]);
    assert_eq!(why(Jpeg, &scanless), "it has no scan before its EOI");
    // Bytes after the EOI are never read; an EXIF thumbnail's own EOI
    // inside an APP1 segment is not the end.
    let mut trailing = jpeg.clone();
    trailing.extend_from_slice(b"\xFF\xD8 junk");
    assert_eq!(check(Jpeg, &trailing), Ok(()));
    let mut app1 = jpeg[..2].to_vec();
    app1.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x06, 0xFF, 0xD9, 0xFF, 0xD9]);
    app1.extend_from_slice(&jpeg[2..]);
    assert_eq!(check(Jpeg, &app1), Ok(()));
}

#[test]
fn a_jpeg_scan_uses_tables_it_defines() {
    let jpeg = bytes(JPEG);
    // Drop the DHT segments: the scan's tables are never defined.
    let mut j = jpeg.clone();
    while let Some(at) = j.windows(2).position(|w| w == [0xFF, 0xC4]) {
        let len = usize::from(u16::from_be_bytes([j[at + 2], j[at + 3]]));
        j.drain(at..at + 2 + len);
    }
    assert_eq!(
        why(Jpeg, &j),
        "a scan uses a Huffman table it never defines"
    );
}

#[test]
fn a_gif_is_walked_to_its_trailer() {
    let gif = bytes(GIF);
    let n = gif.len();
    assert_eq!(
        why(Gif, &gif[..n - 1]),
        "it ends inside its block list (no trailer)"
    );
    let mut trailing = gif.clone();
    trailing.extend_from_slice(b"junk");
    assert_eq!(check(Gif, &trailing), Ok(()));
    // The trailer straight after the header's colour table.
    let mut empty = gif[..19].to_vec();
    empty.push(0x3B);
    assert_eq!(why(Gif, &empty), "it has no image before its trailer");
    // A zero-sized screen.
    let mut zero = gif.clone();
    zero[6..8].copy_from_slice(&[0, 0]);
    assert_eq!(why(Gif, &zero), "its logical screen has no pixels");
}

#[test]
fn a_gifs_first_image_is_checked_as_stb_image_checks_it() {
    let gif = bytes(GIF);
    // The image descriptor starts at 27 (after the GCE): its width at 32,
    // the LZW code size at 37, the first data byte at 39.
    let mut outside = gif.clone();
    outside[32] = 2;
    assert_eq!(
        why(Gif, &outside),
        "its first image lies outside its logical screen"
    );
    let mut size = gif.clone();
    size[37] = 13;
    assert_eq!(why(Gif, &size), "its LZW code size is 13, above 12");
    // The first LZW code (code size 2, so 3 bits) is 4, the clear code:
    // make it 0.
    let mut code = gif.clone();
    code[39] = 0x40;
    assert_eq!(
        why(Gif, &code),
        "its first image's LZW data does not start with a clear code"
    );
}

#[test]
fn a_bmp_needs_a_header_and_compression_stb_image_takes() {
    let bmp = bytes(BMP);
    let mut header = bmp.clone();
    header[14] = 64;
    assert_eq!(
        why(Bmp, &header),
        "its DIB header is 64 bytes, a size stb_image does not take"
    );
    for (compression, reason) in [
        (
            1,
            "it is run-length encoded, which stb_image does not decode",
        ),
        (
            2,
            "it is run-length encoded, which stb_image does not decode",
        ),
        (3, "it has bitfields at 24 bits per pixel"),
        (4, "its compression 4 is not one stb_image takes"),
        (5, "its compression 5 is not one stb_image takes"),
    ] {
        let mut c = bmp.clone();
        c[30] = compression;
        assert_eq!(why(Bmp, &c), reason, "{compression}");
    }
    let mut planes = bmp.clone();
    planes[26] = 2;
    assert_eq!(why(Bmp, &planes), "it does not have one plane");
}

#[test]
fn a_bmp_needs_its_pixels() {
    let bmp = bytes(BMP);
    let n = bmp.len();
    // The last row's padding is not needed; its pixels are.
    assert_eq!(check(Bmp, &bmp[..n - 2]), Ok(()));
    assert_eq!(
        why(Bmp, &bmp[..n - 3]),
        "it ends before its last row of pixels"
    );
    let mut zero = bmp.clone();
    zero[18..22].copy_from_slice(&[0; 4]);
    assert_eq!(why(Bmp, &zero), "it has no pixels");
    // A 1-bit BMP whose offset leaves no room for its palette.
    let mut palette = bytes(BMP_1BIT);
    palette[10] = 54;
    assert_eq!(
        why(Bmp, &palette),
        "it has 1 bits per pixel and no usable palette"
    );
}
