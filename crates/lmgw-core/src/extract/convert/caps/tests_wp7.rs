//! The rows WP7 added: LibreOffice, pandoc, ffmpeg and the speech model.

use super::super::tools::ToolFacts;
use super::*;

fn id(r: &Resolution) -> Option<&'static str> {
    r.handler.map(|h| h.id)
}

/// LibreOffice found with only these parts.
#[cfg(unix)]
fn with_parts(facts: ToolFacts, parts: &[&str]) -> ToolFacts {
    use super::super::tools::Fact;
    use lmgw_api_types::documents::{ExtraKind, ToolExtra};

    let mut facts = facts.with_found(ToolId::LibreOffice);
    if let Some(t) = facts.tools.get_mut(&ToolId::LibreOffice) {
        if let Fact::Found(f) = &mut t.fact {
            f.extras = vec![ToolExtra {
                kind: ExtraKind::Components,
                present: parts.iter().map(|p| p.to_string()).collect(),
                absent: Vec::new(),
            }];
        }
    }
    facts
}

#[cfg(unix)]
#[test]
fn text_markups_need_pandoc_and_fall_back_to_text_naming_it() {
    let none = ToolFacts::all_missing();
    for f in MARKUPS {
        let r = resolve(*f, &none);
        assert_eq!(id(&r), Some("markup.text"), "{f:?}");
        assert_eq!(r.passed_over, vec![ToolId::Pandoc, ToolId::Reader], "{f:?}");
    }
    let both = none.with_found(ToolId::Pandoc).with_found(ToolId::Reader);
    let r = resolve(Format::Latex, &both);
    assert_eq!(id(&r), Some("markup.pandoc"));
    assert!(r.passed_over.is_empty());
}

#[cfg(unix)]
#[test]
fn a_word_file_with_equations_goes_to_pandoc_or_to_the_native_reader_without_them() {
    let none = ToolFacts::all_missing().with_found(ToolId::Reader);
    let r = resolve(Format::DocxMath, &none);
    assert_eq!(id(&r), Some("docx_math.native"));
    assert_eq!(r.passed_over, vec![ToolId::Pandoc]);
    let r = resolve(Format::DocxMath, &none.with_found(ToolId::Pandoc));
    assert_eq!(id(&r), Some("docx_math.pandoc"));
}

#[cfg(unix)]
#[test]
fn rtf_is_pandocs_then_libreoffices_then_names_pandoc() {
    let reader = ToolFacts::all_missing().with_found(ToolId::Reader);
    let r = resolve(Format::Rtf, &reader);
    assert_eq!((id(&r), r.missing.clone()), (None, vec![ToolId::Pandoc]));
    let lo = with_parts(reader.clone(), &["Writer"]);
    let r = resolve(Format::Rtf, &lo);
    assert_eq!(id(&r), Some("rtf.libreoffice"));
    assert_eq!(r.passed_over, vec![ToolId::Pandoc]);
    let r = resolve(Format::Rtf, &lo.with_found(ToolId::Pandoc));
    assert_eq!(id(&r), Some("rtf.pandoc"));
}

#[cfg(unix)]
#[test]
fn old_word_needs_writer_and_old_powerpoint_impress() {
    let reader = ToolFacts::all_missing().with_found(ToolId::Reader);
    let writer = with_parts(reader.clone(), &["Writer"]);
    assert_eq!(
        id(&resolve(Format::Doc, &writer)),
        Some("legacy.libreoffice")
    );
    let r = resolve(Format::Ppt, &writer);
    assert_eq!(
        (id(&r), r.missing.clone()),
        (None, vec![ToolId::LibreOffice])
    );
    let both = with_parts(reader.clone(), &["Writer", "Impress"]);
    assert_eq!(id(&resolve(Format::Ppt, &both)), Some("legacy.libreoffice"));
    // A probe that read no parts (a test's fake) is taken at its word.
    let fake = reader.with_found(ToolId::LibreOffice);
    assert_eq!(id(&resolve(Format::Ppt, &fake)), Some("legacy.libreoffice"));
}

#[test]
fn audio_is_the_models_and_video_needs_ffmpeg() {
    let none = ToolFacts::all_missing();
    assert_eq!(id(&resolve(Format::Audio, &none)), Some("audio.asr"));
    let v = resolve(Format::Video, &none);
    if cfg!(unix) {
        assert_eq!(
            (id(&v), v.missing.clone()),
            (None, vec![ToolId::Ffmpeg, ToolId::Reader])
        );
        let with = none.with_found(ToolId::Ffmpeg).with_found(ToolId::Reader);
        assert_eq!(id(&resolve(Format::Video, &with)), Some("video.ffmpeg"));
    } else {
        assert_eq!(v.no_reader, Some(TOOLS_NOT_RUN_ON_WINDOWS));
    }
}

#[test]
fn markups_are_named_by_their_files_names() {
    assert_eq!(Format::markup_by_name("paper.TEX"), Some(Format::Latex));
    assert_eq!(Format::markup_by_name("README.rst"), Some(Format::Rst));
    assert_eq!(Format::markup_by_name("notes.org"), Some(Format::Org));
    assert_eq!(Format::markup_by_name("page.wiki"), Some(Format::MediaWiki));
    assert_eq!(Format::markup_by_name("book.dbk"), Some(Format::DocBook));
    assert_eq!(Format::markup_by_name("notes.txt"), None);
    assert_eq!(Format::markup_by_name("tex"), None);
}
