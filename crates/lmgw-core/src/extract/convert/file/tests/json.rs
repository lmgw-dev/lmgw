//! `output: json` of text files, stats alike across outputs, and tables
//! charged before they are written.

use super::*;
use crate::extract::convert::doc::{Block, BlockKind};

fn named(name: &str, bytes: &[u8]) -> Input {
    Input {
        index: 0,
        name: name.into(),
        bytes: Ok(Bytes::copy_from_slice(bytes)),
    }
}

fn json() -> Plan {
    plan_of(ConvertOptions {
        output: Output::Json,
        ..Default::default()
    })
}

/// Code, YAML, CSV and plain text keep their text verbatim in json: their
/// indentation, `# comment` lines and numbered lines are no markdown.
#[tokio::test]
async fn a_text_file_that_is_no_markdown_is_one_block_as_it_is() {
    let none = ToolFacts::all_missing();
    let text = "# a comment\ndef f():\n    return 1\n\n1. not a list\n  - key: value\n";
    for name in [
        "example.py",
        "example.yaml",
        "example.csv",
        "notes.txt",
        "example.bin",
    ] {
        let d = convert(named(name, text.as_bytes()), &json(), &none)
            .await
            .doc;
        let v = serde_json::to_value(d.structure.unwrap()).unwrap();
        let blocks = v[0]["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 1, "{name}: {blocks:?}");
        assert_eq!(blocks[0]["kind"], "paragraph");
        assert_eq!(blocks[0]["text"], text, "{name}");
        assert_eq!(d.stats.headings, [0; 6]);
    }
    // A markdown file is read as markdown.
    let d = convert(named("NOTES.MD", text.as_bytes()), &json(), &none)
        .await
        .doc;
    assert_eq!(d.stats.headings[0], 1);
}

/// The same deck counts its slides' headings the same in every output.
#[tokio::test]
async fn office_stats_count_the_same_in_markdown_and_json() {
    let none = ToolFacts::all_missing();
    let deck = test_files::pptx(&[&["Hello"], &["World"]]);
    let md = convert(input(deck.clone()), &plan_of(Default::default()), &none)
        .await
        .doc;
    let js = convert(input(deck), &json(), &none).await.doc;
    assert_eq!(md.stats.headings, js.stats.headings);
    assert_eq!(md.stats.headings[1], 2);
}

/// A table claiming a vast grid with few cells: charged (and refused) by
/// its grid before a row is written, in every output that writes it.
#[test]
fn a_vast_table_is_refused_before_its_grid_is_written() {
    use crate::extract::convert::doc::{Cell, Table};
    let table = Table {
        rows: 100_000,
        cols: 100_000,
        cells: vec![Cell {
            row: 0,
            col: 0,
            row_span: 1,
            col_span: 1,
            header: true,
            text: "x".into(),
        }],
    };
    let doc = Doc {
        parts: vec![Part {
            kind: PartKind::Page,
            number: 1,
            name: None,
            size: None,
            source: Some(Source::TextLayer),
            blocks: vec![Block::new(
                BlockKind::Table(table),
                String::new(),
                None,
                Source::TextLayer,
            )],
        }],
    };
    let tables_off = plan_of(ConvertOptions {
        output: Output::Json,
        tables: false,
        ..Default::default()
    });
    for plan in [plan_of(Default::default()), tables_off] {
        let budget = OutputBudget::request();
        let started = std::time::Instant::now();
        let Err((code, why)) = output(Body::Doc(doc.clone()), &plan, &budget, 0) else {
            panic!("a 10^10-slot grid fits no budget");
        };
        assert_eq!(code, ErrorCode::OutputTooLarge);
        assert!(why.contains("MAX_UNCOMPRESSED_BYTES"), "{why}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
    // Json with tables keeps the cells sparse: nothing to charge.
    let out = output(Body::Doc(doc), &json(), &OutputBudget::request(), 0).unwrap();
    assert!(out.structure.is_some());
}
