use lmgw_api_types::decision_lab::{OptionDraft, QuestionKind, ScoreLabels};

use super::*;

fn questions() -> Vec<QuestionDraft> {
    vec![
        QuestionDraft {
            id: "team".into(),
            instructions: "Which team?".into(),
            kind: QuestionKind::Choice {
                options: ["billing", "technical"]
                    .map(|n| OptionDraft {
                        name: n.into(),
                        description: String::new(),
                    })
                    .to_vec(),
            },
        },
        QuestionDraft {
            id: "angry".into(),
            instructions: "Upset?".into(),
            kind: QuestionKind::Noul {
                yes: String::new(),
                no: String::new(),
            },
        },
    ]
}

fn mapping(state: &str, labels: &[(&str, &str)]) -> BatchMapping {
    BatchMapping {
        state: state.into(),
        state_mode: StateMode::Text,
        labels: labels.iter().map(|(q, c)| (*q, c.to_string())).collect(),
        ..Default::default()
    }
}

/// A score question on a scale written "1" to "5".
fn stars() -> QuestionDraft {
    QuestionDraft {
        id: "stars".into(),
        instructions: "How happy?".into(),
        kind: QuestionKind::Score {
            levels: ["1", "2", "3", "4", "5"].map(String::from).to_vec(),
        },
    }
}

#[test]
fn a_csv_keeps_quoted_line_breaks_and_names_each_rows_line() {
    let text = "\u{feff}text,team,angry\r\n\"one, \"\"two\"\"\nthree\",billing,yes\r\nfour,technical,0\r\n";
    let t = parse(BatchFormat::Csv, text);
    assert_eq!(t.columns, ["text", "team", "angry"]);
    assert!(t.problems.is_empty(), "{:?}", t.problems);
    assert_eq!(t.rows.len(), 2);
    assert_eq!(t.rows[0].cells[0], "one, \"two\"\nthree");
    assert_eq!((t.rows[0].line, t.rows[1].line), (2, 4));

    let items = t
        .items(
            &questions(),
            &mapping("text", &[("team", "team"), ("angry", "angry")]),
        )
        .unwrap();
    assert_eq!(items[0].state, "one, \"two\"\nthree");
    assert_eq!(
        items[1].expected.0,
        [
            ("team".to_string(), "technical".to_string()),
            ("angry".into(), "no".into())
        ]
    );
}

#[test]
fn every_unknown_label_is_named_with_its_row_line_and_value() {
    let text = "text,team\nfirst,billing\nsecond,biling\nthird,\nfourth,Sales\n";
    let t = parse(BatchFormat::Csv, text);
    let problems = t
        .items(&questions(), &mapping("text", &[("team", "team")]))
        .unwrap_err();
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert_eq!(
        (
            problems[0].row,
            problems[0].line,
            problems[0].value.as_deref()
        ),
        (Some(2), Some(3), Some("biling"))
    );
    assert!(
        problems[0]
            .message
            .starts_with("row 2 (line 3), column 'team': 'biling' is not an option of 'team'"),
        "{}",
        problems[0].message
    );
    assert_eq!(problems[1].row, Some(4));
    assert!(
        problems
            .iter()
            .all(|p| p.kind == BatchProblemKind::Label && p.question.as_deref() == Some("team")),
        "{problems:?}"
    );
    let all = message(&problems);
    assert!(
        all.contains("2 problems") && all.contains("'Sales'"),
        "{all}"
    );
}

#[test]
fn a_mapping_names_columns_the_file_has_and_questions_the_set_has() {
    let t = parse(BatchFormat::Csv, "a,a,b\n1,2,3\n");
    let problems = t
        .items(
            &questions(),
            &mapping("text", &[("urgency", "b"), ("team", "a")]),
        )
        .unwrap_err();
    let messages: Vec<&str> = problems.iter().map(|p| p.message.as_str()).collect();
    assert!(
        messages[0].contains("the column 'text' is not in the file"),
        "{messages:?}"
    );
    assert!(
        messages[1].contains("'urgency' is not a question"),
        "{messages:?}"
    );
    assert!(messages[2].contains("2 columns named 'a'"), "{messages:?}");
    let tags: Vec<(BatchProblemKind, Option<&str>)> = problems
        .iter()
        .map(|p| (p.kind, p.question.as_deref()))
        .collect();
    assert_eq!(
        tags,
        [
            (BatchProblemKind::Mapping, None),
            (BatchProblemKind::Mapping, Some("urgency")),
            (BatchProblemKind::Mapping, Some("team")),
        ]
    );
}

#[test]
fn a_row_with_another_field_count_refuses_the_file() {
    let t = parse(BatchFormat::Csv, "text,team\nok,billing\nshort\n");
    assert_eq!(t.rows.len(), 1);
    assert_eq!(t.problems.len(), 1);
    assert_eq!(t.problems[0].line, Some(3));
    assert!(
        t.problems[0]
            .message
            .contains("1 fields where the header has 2"),
        "{:?}",
        t.problems
    );
    assert!(t.items(&questions(), &mapping("text", &[])).is_err());
}

#[test]
fn a_jsonl_object_state_is_kept_as_written_and_null_is_no_label() {
    let text = concat!(
        "{\"log\": {\"b\": 1, \"a\": [2]}, \"team\": \"billing\", \"angry\": true}\n",
        "\n",
        "{\"team\": null, \"log\": {\"z\": 0}, \"angry\": 0, \"extra\": 5}\n",
    );
    let t = parse(BatchFormat::Jsonl, text);
    assert!(t.problems.is_empty(), "{:?}", t.problems);
    assert_eq!(t.columns, ["log", "team", "angry", "extra"]);
    assert_eq!(t.rows[1].line, 3);
    assert_eq!(
        t.rows[0].cells,
        ["{\"b\": 1, \"a\": [2]}", "billing", "true", ""]
    );
    let cols = t.column_views();
    assert!(cols[0].json && !cols[1].json && !cols[3].json, "{cols:?}");

    let mut m = mapping("log", &[("team", "team"), ("angry", "angry")]);
    m.state_mode = StateMode::Json;
    let items = t.items(&questions(), &m).unwrap();
    assert_eq!(items[0].state, "{\"b\": 1, \"a\": [2]}");
    assert_eq!(
        items[0].expected.get("angry").map(String::as_str),
        Some("yes")
    );
    assert_eq!(items[1].expected.get("team"), None, "null is no label");
    assert_eq!(
        items[1].expected.get("angry").map(String::as_str),
        Some("no")
    );
}

#[test]
fn a_json_state_that_does_not_parse_and_a_line_that_is_no_object_are_problems() {
    let t = parse(BatchFormat::Jsonl, "{\"s\": \"{oops\"}\n[1, 2]\n");
    assert_eq!(t.problems.len(), 1);
    assert!(
        t.problems[0]
            .message
            .starts_with("line 2: not a JSON object"),
        "{:?}",
        t.problems
    );
    let mut m = mapping("s", &[]);
    m.state_mode = StateMode::Json;
    let problems = t.items(&questions(), &m).unwrap_err();
    assert!(
        problems[1].message.contains("the state is not JSON"),
        "{problems:?}"
    );
    assert_eq!(
        (
            problems[0].kind,
            problems[1].kind,
            problems[1].question.as_deref()
        ),
        (BatchProblemKind::File, BatchProblemKind::State, None)
    );
}

#[test]
fn a_preview_shows_the_top_and_counts_every_row() {
    let mut text = String::from("text\n");
    for i in 0..30 {
        text.push_str(&format!("row {i}\n"));
    }
    let p = preview(&BatchPreview {
        format: BatchFormat::Csv,
        text,
        ..Default::default()
    });
    assert_eq!(p.rows, 30);
    assert_eq!(p.sample.len(), PREVIEW_ROWS as usize);
    assert_eq!(p.sample[0], ["row 0"]);
    assert!(p.problems.is_empty());
}

#[test]
fn a_score_column_of_indices_is_read_as_indices_never_value_by_value() {
    // 0-based indices on a "1" to "5" scale: "0" is no level's text, so the
    // whole column is indices, and "1" is level 1, not the text of level 0.
    let t = parse(BatchFormat::Csv, "text,stars\na,0\nb,1\nc,\nd,4\n");
    let items = t
        .items(&[stars()], &mapping("text", &[("stars", "stars")]))
        .unwrap();
    let read: Vec<Option<&str>> = items
        .iter()
        .map(|i| i.expected.get("stars").map(String::as_str))
        .collect();
    assert_eq!(read, [Some("0"), Some("1"), None, Some("4")]);
}

#[test]
fn a_score_column_both_readings_fit_is_refused_until_the_mapping_says_which() {
    let t = parse(BatchFormat::Csv, "text,stars\na,1\nb,2\nc,4\n");
    let mut m = mapping("text", &[("stars", "stars")]);
    let problems = t.items(&[stars()], &m).unwrap_err();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(
        (problems[0].row, problems[0].value.as_deref()),
        (Some(1), Some("1"))
    );
    assert!(
        problems[0]
            .message
            .contains("set score_labels for 'stars' to text or index"),
        "{}",
        problems[0].message
    );
    // The page keys its reading select on this, not on the message.
    assert_eq!(
        (problems[0].kind, problems[0].question.as_deref()),
        (BatchProblemKind::ScoreLabelsNeeded, Some("stars"))
    );
    m.score_labels = [("stars", ScoreLabels::Text)].into_iter().collect();
    let items = t.items(&[stars()], &m).unwrap();
    assert_eq!(
        items[2].expected.get("stars").map(String::as_str),
        Some("3")
    );
    m.score_labels = [("stars", ScoreLabels::Index)].into_iter().collect();
    let items = t.items(&[stars()], &m).unwrap();
    assert_eq!(
        items[2].expected.get("stars").map(String::as_str),
        Some("4")
    );
    // Said to be indices, a "5" is no index of five levels.
    let t = parse(BatchFormat::Csv, "text,stars\na,5\n");
    let problems = t.items(&[stars()], &m).unwrap_err();
    assert!(
        problems[0]
            .message
            .contains("'5' is no level index of 'stars' (0 to 4)"),
        "{problems:?}"
    );
    // Only a score's labels have a reading.
    m.score_labels = [("team", ScoreLabels::Index)].into_iter().collect();
    let problems = t
        .items(&questions(), &mapping_with(&m, "text"))
        .unwrap_err();
    assert!(
        problems[0].message.contains("'team' is a choice question"),
        "{problems:?}"
    );
}

fn mapping_with(m: &BatchMapping, state: &str) -> BatchMapping {
    BatchMapping {
        state: state.into(),
        labels: Default::default(),
        ..m.clone()
    }
}

#[test]
fn a_score_column_that_mixes_texts_and_indices_names_the_odd_rows() {
    let urgency = QuestionDraft {
        id: "urgency".into(),
        instructions: String::new(),
        kind: QuestionKind::Score {
            levels: ["later", "today", "now"].map(String::from).to_vec(),
        },
    };
    let t = parse(
        BatchFormat::Csv,
        "text,u\na,today\nb,2\nc,now\nd,later\ne,soon\n",
    );
    let problems = t
        .items(&[urgency], &mapping("text", &[("urgency", "u")]))
        .unwrap_err();
    let named: Vec<(Option<u64>, Option<&str>)> = problems
        .iter()
        .map(|p| (p.row, p.value.as_deref()))
        .collect();
    // Row 2 is the one index among texts; row 5 is no level either way.
    assert_eq!(named, [(Some(2), Some("2")), (Some(5), Some("soon"))]);
    assert_eq!(
        (problems[0].kind, problems[1].kind),
        (BatchProblemKind::ScoreLabelsNeeded, BatchProblemKind::Label)
    );
    assert!(
        problems[0].message.contains(
            "'2' is a level index of 'urgency', but the column's other values are level \
             texts (as at row 1)"
        ),
        "{}",
        problems[0].message
    );
    assert!(
        problems[1]
            .message
            .contains("'soon' is no level of 'urgency'"),
        "{}",
        problems[1].message
    );
}

#[test]
fn a_mapping_without_a_state_column_or_with_an_empty_name_is_refused() {
    // pandas writes its index as a column with an empty header.
    let t = parse(BatchFormat::Csv, ",text,team\n0,hello,billing\n");
    let problems = t
        .items(&questions(), &mapping("", &[("team", "")]))
        .unwrap_err();
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(
        problems[0]
            .message
            .starts_with("state: the mapping names no state column"),
        "{}",
        problems[0].message
    );
    assert!(
        problems[1]
            .message
            .contains("labels of 'team': the mapping names a column with an empty name"),
        "{}",
        problems[1].message
    );
    let mut m = mapping("text", &[]);
    m.key = Some(" ".into());
    let problems = t.items(&questions(), &m).unwrap_err();
    assert!(
        problems[0]
            .message
            .starts_with("key: the mapping names a column"),
        "{problems:?}"
    );
}

#[test]
fn a_key_column_is_kept_per_item_and_an_empty_value_is_no_key() {
    let t = parse(BatchFormat::Csv, "id,text\nT-1,hello\n,again\n");
    let mut m = mapping("text", &[]);
    m.key = Some("id".into());
    let items = t.items(&questions(), &m).unwrap();
    assert_eq!(
        items.iter().map(|i| i.key.as_deref()).collect::<Vec<_>>(),
        [Some("T-1"), None]
    );
    m.key = Some("ticket".into());
    let problems = t.items(&questions(), &m).unwrap_err();
    assert!(
        problems[0]
            .message
            .contains("key: the column 'ticket' is not in the file"),
        "{problems:?}"
    );
}
