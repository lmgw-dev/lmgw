//! The PDF handler's own rules after the model stage (WP6 review round).

use super::super::super::doc::{Part, PartKind};
use super::super::super::options;
use super::*;
use lmgw_api_types::documents::InstallPlatform;

fn page(number: u32, source: Source) -> Part {
    Part {
        kind: PartKind::Page,
        number,
        name: None,
        size: None,
        source: Some(source),
        blocks: Vec::new(),
    }
}

/// Review nit 5: a page the page-reading model read again is not
/// counted as one a repeated line was left out of.
#[test]
fn header_removed_counts_only_the_pages_the_tools_read() {
    let line = Repeated {
        text: "Example Annual Report".into(),
        band: Band::Top,
        numbered: false,
    };
    let plan = options::plan(&Default::default(), InstallPlatform::Fedora)
        .unwrap()
        .0;
    let mut a = nothing_picked(3, &plan);
    a.doc.parts = vec![
        page(1, Source::TextLayer),
        page(2, Source::Vision),
        page(3, Source::TextLayer),
    ];
    a.repeated = vec![line];
    a.removed = vec![vec![0], vec![0], vec![0]];
    let (lines, by_model) = a.repeated_now();
    assert_eq!((lines.len(), lines[0].1, by_model), (1, 2, 1));
    let w = repeated_warning(&lines, by_model);
    assert!(w.message.contains("(top, 2 pages)"), "{}", w.message);
    assert!(
        w.message
            .contains("1 more page(s) with such lines were read by the page-reading model"),
        "{}",
        w.message
    );
    // Every such page read by the model: nothing is claimed.
    for p in &mut a.doc.parts {
        p.source = Some(Source::Vision);
    }
    let (lines, by_model) = a.repeated_now();
    assert!(lines.is_empty());
    assert_eq!(by_model, 3);
    // No model: as before, no word about one.
    let none = repeated_warning(&[], 0);
    assert!(!none.message.contains("page-reading model"));
}

/// S6: the model's reason replaces "nothing reads scanned pages", and
/// the calls made stay with the failed file.
#[test]
fn a_file_the_model_left_unread_fails_with_the_model_s_code_and_its_calls() {
    let stage = model_help::pdf::Stage {
        warnings: vec![warning(
            WarningCode::ModelHeld,
            "page 1 would have been read by the page-reading model, which is paused".into(),
            vec![1],
        )],
        calls: 2,
        partial: true,
        failure: Some(ErrorCode::ModelHeld),
        ..Default::default()
    };
    let mut f = fail(
        Some(Format::Pdf),
        ErrorCode::NoReader,
        "page 1 has no text layer, so it was not read: not read as text yet",
    );
    model_failure(&stage, &mut f);
    assert_eq!(f.error.code, ErrorCode::ModelHeld);
    assert!(f
        .error
        .message
        .starts_with("no page of this PDF has a text layer; page 1"));
    assert!(!f.error.message.contains("not read as text yet"));
    assert_eq!(f.model.0, 2);
    // Another failure (the asked-for pages are not in the file) keeps
    // its own code; the stage's words are added.
    let mut f = fail(
        Some(Format::Pdf),
        ErrorCode::PagesOutOfRange,
        "pages 9 are not there",
    );
    model_failure(&stage, &mut f);
    assert_eq!(f.error.code, ErrorCode::PagesOutOfRange);
    assert!(f.error.message.contains("; page 1 would have been read"));
    // A stage that asked nothing changes nothing.
    let mut f = fail(Some(Format::Pdf), ErrorCode::NoReader, "as it was");
    model_failure(&Default::default(), &mut f);
    assert_eq!(
        (f.error.code, f.error.message.as_str()),
        (ErrorCode::NoReader, "as it was")
    );
}
