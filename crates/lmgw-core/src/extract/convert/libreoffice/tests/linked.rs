//! What a docx LibreOffice wrote says of a picture it linked instead of
//! embedding (26.8.3, measured 2026-11-20).

use super::*;

/// Whether a docx has an external picture relationship to `path`.
pub(super) fn links(zip: &[u8], path: &Path) -> bool {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    let mut rels = String::new();
    std::io::Read::read_to_string(
        &mut z.by_name("word/_rels/document.xml.rels").unwrap(),
        &mut rels,
    )
    .unwrap();
    let name = path.to_string_lossy();
    rels.split("<Relationship ")
        .any(|r| r.contains("TargetMode=\"External\"") && r.contains(&*name))
}

/// lmgw's own conversion of a docx LibreOffice wrote, with pictures
/// embedded and referenced, markdown and json: nothing of it holds
/// `picture` (base64, a data URI, an artifact link or a picture with
/// bytes), and the text is there.
pub(super) async fn answered_without(docx: &[u8], picture: &[u8]) {
    use base64::Engine as _;
    use lmgw_api_types::documents::convert::{ConvertOptions, Output, Pictures};
    let b64 = base64::engine::general_purpose::STANDARD.encode(picture);
    let facts = crate::extract::convert::tools::ToolFacts::all_missing();
    for pictures in [Pictures::Embedded, Pictures::Referenced] {
        for output in [Output::Markdown, Output::Json] {
            let plan = crate::extract::convert::options::plan(
                &ConvertOptions {
                    pictures,
                    output,
                    ..Default::default()
                },
                lmgw_api_types::documents::InstallPlatform::Fedora,
            )
            .unwrap()
            .0;
            let input = crate::extract::convert::file::Input {
                index: 0,
                name: "converted.docx".into(),
                bytes: Ok(Bytes::from(docx.to_vec())),
            };
            let d = crate::extract::convert::file::convert(
                input,
                &plan,
                &facts,
                &std::sync::Arc::new(crate::extract::convert::budget::OutputBudget::request()),
            )
            .await
            .doc;
            let json = serde_json::to_string(&d).unwrap();
            let content = d.content.as_deref().unwrap_or_default();
            assert!(
                json.contains("Probe text."),
                "{pictures:?} {output:?}: {json}"
            );
            assert!(
                !json.contains(&b64[..64]),
                "{pictures:?} {output:?}: {json}"
            );
            assert!(
                !content.contains("data:image") && !content.contains("artifacts/"),
                "{pictures:?} {output:?}: {content}"
            );
            assert!(
                d.pictures
                    .iter()
                    .all(|p| p.data_base64.is_none() && p.file.is_none() && p.bytes.is_none()),
                "{pictures:?} {output:?}: {:?}",
                d.pictures
            );
        }
    }
}
