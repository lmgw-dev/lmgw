//! Pictures a Word or PowerPoint file links instead of embedding (LibreOffice
//! 26.8.3 writes `INCLUDEPICTURE` targets this way; a caller can upload any
//! such file): a `r:link` relationship with `TargetMode="External"` that
//! names a real file on this machine. lmgw's readers only ever read zip
//! entries, so the answer holds none of that file, with every picture mode
//! and output. Needs no tool, so it runs on every platform.

use base64::Engine as _;
use lmgw_api_types::documents::convert::{Pictures, WarningCode};

use super::pictures::mode;
use super::*;

/// A real PNG, written to a directory of its own; the file's absolute path
/// as a `file:` URL, and the PNG's bytes.
fn secret(dir: &std::path::Path) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut bytes, 13, 7);
        enc.set_color(png::ColorType::Rgb);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[201u8; 13 * 7 * 3]).unwrap();
        w.finish().unwrap();
    }
    let path = dir.join("secret-outside.png");
    std::fs::write(&path, &bytes).unwrap();
    let p = path.to_string_lossy().replace('\\', "/");
    let url = if p.starts_with('/') {
        format!("file://{p}")
    } else {
        format!("file:///{p}")
    };
    (url, bytes)
}

const CONTENT_TYPES: &[u8] = br#"<Types><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;

fn docx_linking(url: &str) -> Vec<u8> {
    let doc = r#"<w:document xmlns:w="w" xmlns:wp="wp" xmlns:a="a" xmlns:r="r"><w:body><w:p><w:r><w:t>Before.</w:t></w:r></w:p><w:p><w:r><w:drawing><wp:inline><wp:extent cx="3000000" cy="3000000"/><a:blip r:link="rIdX"/></wp:inline></w:drawing></w:r></w:p><w:p><w:r><w:t>Figure 1: Example</w:t></w:r></w:p></w:body></w:document>"#;
    let rels = format!(
        r#"<Relationships><Relationship Id="rIdX" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="{url}" TargetMode="External"/></Relationships>"#
    );
    crate::extract::test_files::zip(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("word/document.xml", doc.as_bytes()),
        ("word/_rels/document.xml.rels", rels.as_bytes()),
    ])
}

fn pptx_linking(url: &str) -> Vec<u8> {
    let slide = r#"<p:sld xmlns:a="a" xmlns:p="p" xmlns:r="r"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Title</a:t></a:r></a:p></p:txBody></p:sp><p:pic><p:blipFill><a:blip r:link="rIdX"/></p:blipFill><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="4000000" cy="3000000"/></a:xfrm></p:spPr></p:pic></p:spTree></p:cSld></p:sld>"#;
    let pres = r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst><p:sldId r:id="rId1"/></p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/></p:presentation>"#;
    let rels = format!(
        r#"<Relationships><Relationship Id="rIdX" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="{url}" TargetMode="External"/></Relationships>"#
    );
    crate::extract::test_files::zip(&[
        (
            "[Content_Types].xml",
            br#"<Types><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>"#,
        ),
        ("ppt/presentation.xml", pres.as_bytes()),
        (
            "ppt/_rels/presentation.xml.rels",
            br#"<Relationships><Relationship Id="rId1" Target="slides/slide1.xml"/></Relationships>"#,
        ),
        ("ppt/slides/slide1.xml", slide.as_bytes()),
        ("ppt/slides/_rels/slide1.xml.rels", rels.as_bytes()),
    ])
}

/// What the answer holds of `secret`: its bytes in base64 anywhere in the
/// answer as JSON, a data URI or an artifact link in the content, or a
/// picture with bytes or a file name.
fn holds_any(d: &ConvertedDocument, secret: &[u8]) -> Option<String> {
    let json = serde_json::to_string(d).unwrap();
    let b64 = base64::engine::general_purpose::STANDARD.encode(secret);
    let content = d.content.as_deref().unwrap_or_default();
    if json.contains(&b64[..64]) {
        return Some("the PNG's base64".into());
    }
    if content.contains("data:image") || content.contains("artifacts/") {
        return Some(format!("a picture in the content: {content}"));
    }
    if let Some(p) = d
        .pictures
        .iter()
        .find(|p| p.data_base64.is_some() || p.file.is_some() || p.bytes.is_some())
    {
        return Some(format!("a picture with bytes: {p:?}"));
    }
    None
}

/// A linked picture is never read from the machine: `r:link` resolves to a
/// zip entry that is not there, whatever the picture mode. What the answer
/// says of it today: the picture keeps its placeholder (a `<!-- image -->`
/// line, a `figure` block in json) with a `pictures_partial` note that the
/// picture is linked, outside the file, and has no bytes.
#[tokio::test]
async fn a_linked_picture_naming_a_local_file_is_never_read_from_the_machine() {
    let dir = tempfile::tempdir().unwrap();
    let (url, png) = secret(dir.path());
    let none = ToolFacts::all_missing();
    let files = [
        ("docx", docx_linking(&url), "Before."),
        ("pptx", pptx_linking(&url), "Title"),
    ];
    for (kind, file, text) in files {
        for pictures in [Pictures::Embedded, Pictures::Referenced] {
            for output in [Output::Markdown, Output::Json] {
                let d = convert(input(file.clone()), &mode(pictures, output), &none)
                    .await
                    .doc;
                assert_eq!(d.status, DocStatus::Converted, "{kind}: {d:?}");
                assert!(
                    d.warnings
                        .iter()
                        .any(|w| w.code == WarningCode::PicturesPartial
                            && w.message.contains("1 linked from outside the file")),
                    "{kind}: the note says the picture is linked: {d:?}"
                );
                assert!(
                    holds_any(&d, &png).is_none(),
                    "{kind} {pictures:?} {output:?}: {:?}: {d:?}",
                    holds_any(&d, &png)
                );
                assert!(
                    serde_json::to_string(&d).unwrap().contains(text),
                    "{kind}: the text still converts: {d:?}"
                );
            }
        }
    }
}
