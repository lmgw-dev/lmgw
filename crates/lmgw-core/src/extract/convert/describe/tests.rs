use lmgw_api_types::documents::convert::{ConvertOptions, Pictures, WarningCode};

use super::*;
use crate::extract::convert::doc::{Block, Figure, Part, PartKind, Source};
use crate::extract::convert::pictures::{self, Picture};

#[test]
fn without_a_chosen_model_descriptions_are_said_and_a_lone_prompt_too() {
    let ask = |describe, prompt: Option<&str>| ConvertOptions {
        describe_pictures: describe,
        picture_prompt: prompt.map(str::to_string),
        ..Default::default()
    };
    let mut notes = Vec::new();
    assert!(plan(&ask(true, Some("Describe.")), &Reader::none(), &mut notes).is_none());
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].code, WarningCode::ModelNotChosen);
    assert_eq!(notes[0].option.as_deref(), Some("describe_pictures"));
    let mut notes = Vec::new();
    assert!(plan(&ask(false, Some("Describe.")), &Reader::none(), &mut notes).is_none());
    assert_eq!(notes[0].code, WarningCode::OptionNotApplicable);
    let mut notes = Vec::new();
    assert!(plan(&ask(false, None), &Reader::none(), &mut notes).is_none());
    assert!(notes.is_empty());
}

/// The picture's line is the picture alone in every mode: its description
/// is written after the picture and its caption (`place`).
#[test]
fn the_picture_line_is_the_picture_alone() {
    let mut p = Picture::new(1, None, Source::Native);
    p.image = Some(pictures::Image {
        bytes: bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
        mime: "image/png",
        ext: "png",
        width_px: Some(4),
        height_px: Some(4),
    });
    p.description = Some("A red square.".into());
    for mode in [
        Pictures::Placeholder,
        Pictures::Embedded,
        Pictures::Referenced,
    ] {
        let mut out = String::new();
        pictures::line(mode, 0, Some(&p), &mut out);
        assert!(!out.contains("red square"), "{mode:?}: {out}");
    }
}

fn block(kind: BlockKind, text: &str) -> Block {
    Block::new(kind, text.into(), None, Source::TextLayer)
}

fn figure(index: u32) -> Block {
    let mut fig = block(BlockKind::Figure, "");
    fig.figure = Some(Box::new(Figure {
        index,
        caption: None,
        description: None,
    }));
    fig
}

fn page(blocks: Vec<Block>) -> Doc {
    Doc {
        parts: vec![Part {
            kind: PartKind::Page,
            number: 1,
            name: None,
            size: None,
            source: Some(Source::TextLayer),
            blocks,
        }],
    }
}

/// S4: image, caption, description, in markdown and in text (docling's
/// picture item holds its caption, and an item's meta comes after it);
/// json carries it on the figure block.
#[test]
fn the_description_follows_the_picture_and_its_caption() {
    let mut doc = page(vec![
        block(BlockKind::Paragraph, "Before."),
        figure(0),
        block(BlockKind::Caption, "Figure 1: Example chart"),
        figure(1),
        block(BlockKind::Paragraph, "After."),
    ]);
    let mut found = Found::default();
    let mut captioned = Picture::new(1, None, Source::TextLayer);
    captioned.caption = Some("Figure 1: Example chart".into());
    captioned.description = Some("# A chart of example sales.".into());
    let mut bare = Picture::new(1, None, Source::TextLayer);
    bare.description = Some("A second example.".into());
    found.pictures = vec![captioned, bare];
    let render = |doc: &Doc, text: bool| {
        let map = after_blocks(doc, &found, text);
        let after = |b: &Block| map.get(&(std::ptr::from_ref(b) as usize)).cloned();
        if text {
            crate::extract::convert::doc::render::text_after(doc, "\n\n", &after)
        } else {
            let line = |_: &Block, out: &mut String| out.push_str("<!-- image -->");
            crate::extract::convert::doc::render::markdown_after(
                doc,
                crate::extract::convert::doc::render::TableStyle::Padded,
                "\n\n",
                &line,
                &after,
            )
        }
    };
    assert_eq!(
        render(&doc, false),
        "Before.\n\n<!-- image -->\n\nFigure 1: Example chart\n\n\\# A chart of example \
         sales.\n\n<!-- image -->\n\nA second example.\n\nAfter."
    );
    assert_eq!(
        render(&doc, true),
        "Before.\n\nFigure 1: Example chart\n\n# A chart of example sales.\n\nA second \
         example.\n\nAfter."
    );
    stamp(&mut doc, &found);
    let f = doc.parts[0].blocks[1].figure.as_ref().unwrap();
    assert_eq!(
        f.description.as_deref(),
        Some("# A chart of example sales.")
    );
    assert_eq!(
        crate::extract::convert::doc::render::text(&doc, "\n\n"),
        "Before.\n\nFigure 1: Example chart\n\nAfter.",
        "plain text without the map writes no description"
    );
}

/// S4 on a reader's markdown (office files): the description after the
/// caption paragraph that follows the marker, else after the picture.
#[test]
fn a_reader_s_markdown_writes_the_description_after_the_caption() {
    use crate::extract::convert::pictures::office;
    let md = format!(
        "Before.\n\n{}\n\nFigure 1: Example chart\n\n{}\n\nAfter.",
        crate::extract::office_media::marker(0),
        crate::extract::office_media::marker(1)
    );
    let mut found = Found::default();
    let mut captioned = Picture::new(1, None, Source::Native);
    captioned.caption = Some("Figure 1: Example chart".into());
    captioned.description = Some("A chart.".into());
    let mut bare = Picture::new(1, None, Source::Native);
    bare.description = Some("A photo.".into());
    found.pictures = vec![captioned, bare];
    found.markers = vec![Some(0), Some(1)];
    let line = |_: u32, out: &mut String| out.push_str("<!-- image -->");
    let said = |i: u32| found.pictures[i as usize].description.clone();
    assert_eq!(
        office::lines(&md, &found, Some(&line), Some(&said)),
        "Before.\n\n<!-- image -->\n\nFigure 1: Example chart\n\nA chart.\n\n<!-- image \
         -->\n\nA photo.\n\nAfter."
    );
    assert_eq!(
        office::lines(&md, &found, None, Some(&said)),
        "Before.\n\nFigure 1: Example chart\n\nA chart.\n\nA photo.\n\nAfter."
    );
}

/// S8: a description's lines cannot become a heading, a comment hiding what
/// follows, a fence, a display formula, a table, a rule or a link
/// definition; the model's control tokens are gone. The structure after it
/// stays the document's.
#[test]
fn a_description_cannot_change_the_document_s_structure() {
    let answer = "# Results\n<!-- hidden\n```\n$$ x^2 $$\n| a | b |\nTitle\n---\n\
                  [x]: https://example.invalid/\n- item\nPlain words.<|im_end|>";
    let cleaned = crate::extract::convert::model_help::answer::without_control(answer);
    assert!(!cleaned.contains("<|im_end|>"));
    let md = paragraph(&cleaned);
    for l in md.lines() {
        for start in ["#", "<", "```", "$$", "|", "---", "[x]:", "- "] {
            assert!(!l.starts_with(start), "{l:?} in {md}");
        }
    }
    assert!(md.ends_with("Plain words."), "{md}");
    // Parsed back as markdown, it is one paragraph and nothing else.
    let doc = crate::extract::convert::doc::from_md::blocks(&format!("{md}\n\nAfter."));
    let kinds: Vec<&BlockKind> = doc.iter().map(|b| &b.kind).collect();
    assert_eq!(
        kinds,
        [&BlockKind::Paragraph, &BlockKind::Paragraph],
        "{md}"
    );
    assert_eq!(doc[1].text, "After.");
}

#[test]
fn the_paragraph_keeps_paragraphs_once_and_escapes_markup() {
    assert_eq!(paragraph("  one\ntwo\n\n\n\nthree  "), "one\ntwo\n\nthree");
    assert_eq!(paragraph("| a | b |"), "\\| a | b |");
}

/// A request's description whose model is an alias on an upstream outside
/// lmgw (nothing listens there).
async fn described() -> Describe {
    use crate::config::{Protocol, UpstreamKind};
    use crate::store::{self, NewAlias, NewUpstream};
    let state = crate::state::AppState::init_for_tests().await.unwrap();
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "example-up".into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::Generic,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 1_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "example-vision".into(),
            upstream_id: up,
            upstream_model_id: "vision-tgt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    let mut s = state.snapshot().settings.clone();
    s.documents.picture_alias = "example-vision".into();
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let reader = Reader::of(&state, &crate::proxy::RequestCtx::default());
    let o = ConvertOptions {
        describe_pictures: true,
        ..Default::default()
    };
    plan(&o, &reader, &mut Vec::new()).unwrap()
}

/// S2: a describe stage that starts after the file's deadline (a page stage
/// before it kept the file going) opens no route and sends nothing; the
/// pictures are named.
#[tokio::test]
async fn a_stage_on_an_expired_clock_opens_nothing() {
    use crate::extract::convert::pool::{self, clock};
    let d = described().await;
    let mut found = Found::default();
    for _ in 0..20 {
        let mut p = Picture::new(1, None, Source::Native);
        p.image = Some(pictures::Image {
            bytes: bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
            mime: "image/png",
            ext: "png",
            width_px: Some(4),
            height_px: Some(4),
        });
        found.pictures.push(p);
    }
    let c = clock::FileClock::new(std::time::Duration::from_millis(1));
    let out = clock::scope(c.clone(), async {
        // The file's first slot starts its clock; its work runs past it.
        drop(pool::global().acquire().await);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(c.expired());
        run(&d, &mut found).await
    })
    .await;
    assert!(!d.chosen.session.was_asked().await, "nothing was opened");
    assert_eq!(out.calls, 0);
    assert!(out.partial);
    assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    assert!(
        out.warnings[0]
            .message
            .contains("document_timeout of 0.001 s had passed before their turn"),
        "{}",
        out.warnings[0].message
    );
}
