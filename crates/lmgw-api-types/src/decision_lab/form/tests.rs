use super::*;
use crate::systemone::{Question, SystemOneRequest};

fn ticket() -> DecisionForm {
    DecisionForm {
        state_mode: StateMode::Text,
        state: "Charged \"twice\"\nplease refund".into(),
        images: Vec::new(),
        questions: vec![
            QuestionDraft {
                id: "team".into(),
                instructions: "Which team?".into(),
                kind: QuestionKind::Choice {
                    options: vec![
                        OptionDraft {
                            name: "technical".into(),
                            description: "bugs".into(),
                        },
                        OptionDraft {
                            name: "billing".into(),
                            description: "refunds".into(),
                        },
                        OptionDraft {
                            name: "other".into(),
                            description: " ".into(),
                        },
                    ],
                },
            },
            QuestionDraft {
                id: "urgency".into(),
                instructions: "How urgent?".into(),
                kind: QuestionKind::Score {
                    levels: vec!["later".into(), "now".into()],
                },
            },
            QuestionDraft {
                id: "angry".into(),
                instructions: "Upset?".into(),
                kind: QuestionKind::Noul {
                    yes: "upset".into(),
                    no: String::new(),
                },
            },
        ],
    }
}

/// The body's text, member by member: `model`, `state`, `questions` in the
/// order entered, ids and options as entered, a blank description `null`,
/// a `noul` with only a yes sending only `true`.
#[test]
fn the_body_is_written_in_the_order_entered() {
    let body = ticket()
        .body("decide/laya", Variant::AsWritten, &[])
        .unwrap();
    assert_eq!(
        body,
        r#"{
  "model": "decide/laya",
  "state": "Charged \"twice\"\nplease refund",
  "questions": {
    "team": {
      "type": "choice",
      "instructions": "Which team?",
      "criteria": {
        "technical": "bugs",
        "billing": "refunds",
        "other": null
      }
    },
    "urgency": {
      "type": "score",
      "instructions": "How urgent?",
      "criteria": [
        "later",
        "now"
      ]
    },
    "angry": {
      "type": "noul",
      "instructions": "Upset?",
      "criteria": {
        "true": "upset"
      }
    }
  }
}"#
    );
    // And it is the wire's request, the options in their order.
    let req: SystemOneRequest = serde_json::from_str(&body).unwrap();
    let Some(Question::Choice(c)) = req.questions.get("team") else {
        panic!("{req:?}")
    };
    assert_eq!(
        c.criteria.keys().collect::<Vec<_>>(),
        ["technical", "billing", "other"]
    );
}

/// A JSON-mode state goes in byte for byte: its spacing, its member order
/// and its number spellings.
#[test]
fn a_json_state_goes_out_verbatim() {
    let mut f = ticket();
    f.state_mode = StateMode::Json;
    f.state = "{\"b\": 1,\n   \"a\": 2.50}".into();
    let body = f.body("m", Variant::AsWritten, &[]).unwrap();
    assert!(
        body.contains("  \"state\": {\"b\": 1,\n   \"a\": 2.50},\n"),
        "{body}"
    );
    f.state = "null".into();
    assert!(f
        .body("m", Variant::AsWritten, &[])
        .unwrap()
        .contains("\"state\": null,"));
    f.state = "{\"b\": ".into();
    assert!(f.body("m", Variant::AsWritten, &[]).is_err());
}

#[test]
fn a_text_state_is_a_json_string() {
    let mut f = ticket();
    f.state = "{\"looks\": \"like json\"}".into();
    let body = f.body("m", Variant::AsWritten, &[]).unwrap();
    assert!(
        body.contains(r#""state": "{\"looks\": \"like json\"}","#),
        "{body}"
    );
}

#[test]
fn the_variants_touch_choice_descriptions_order_and_noul_criteria_only() {
    let f = ticket();
    let plain = f.body("m", Variant::NoDescriptions, &[]).unwrap();
    let req: SystemOneRequest = serde_json::from_str(&plain).unwrap();
    let Some(Question::Choice(c)) = req.questions.get("team") else {
        panic!()
    };
    assert!(c.criteria.iter().all(|(_, d)| d.is_none()), "{plain}");
    let Some(Question::Noul(n)) = req.questions.get("angry") else {
        panic!()
    };
    assert_eq!(n.criteria, None);
    let Some(Question::Score(s)) = req.questions.get("urgency") else {
        panic!()
    };
    assert_eq!(s.criteria, ["later", "now"], "a score keeps its levels");

    let rev = f.body("m", Variant::Reversed, &[]).unwrap();
    let req: SystemOneRequest = serde_json::from_str(&rev).unwrap();
    let Some(Question::Choice(c)) = req.questions.get("team") else {
        panic!()
    };
    assert_eq!(
        c.criteria.keys().collect::<Vec<_>>(),
        ["other", "billing", "technical"]
    );
    assert_eq!(c.criteria.get("billing"), Some(&Some("refunds".into())));
    assert_eq!(
        req.questions.keys().collect::<Vec<_>>(),
        ["team", "urgency", "angry"],
        "question order is answer order"
    );
    let Some(Question::Score(s)) = req.questions.get("urgency") else {
        panic!()
    };
    assert_eq!(
        s.criteria,
        ["later", "now"],
        "reversing a scale would invert it"
    );
}

/// The dispatched body carries each data URL byte for byte as typed, a
/// declared type and its parameters included; the preview is that body with
/// each payload elided, and with the payloads in place it is the body again.
#[test]
fn the_preview_with_payloads_restored_is_the_dispatched_body() {
    // Typed by hand: a type no sniffer would give, and a parameter.
    let url = "data:image/x-receipt;name=receipt.png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==".to_string();
    let f = ticket();
    let sent = f
        .body("m", Variant::AsWritten, std::slice::from_ref(&url))
        .unwrap();
    let without = f.body("m", Variant::AsWritten, &[]).unwrap();
    assert_eq!(
        sent,
        format!(
            "{},\n  \"images\": [\n    \"data:image/x-receipt;name=receipt.png;base64,\
             iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==\"\n  ]\n}}",
            without.strip_suffix("\n}").unwrap()
        ),
        "the URL as typed, after the questions"
    );
    let preview = f
        .body("m", Variant::AsWritten, &[elide_data_url(&url)])
        .unwrap();
    assert!(
        preview.contains("\"data:image/x-receipt;name=receipt.png;base64,… 70 B\""),
        "{preview}"
    );
    assert_eq!(
        preview.replace(&elide_data_url(&url), &url),
        sent,
        "byte for byte once restored"
    );
    let req: SystemOneRequest = serde_json::from_str(&sent).unwrap();
    assert_eq!(req.images, Some(vec![url]));
}

#[test]
fn the_check_names_the_field() {
    let ok = ticket();
    assert_eq!(ok.check(), Ok(()));
    type Edit = Box<dyn Fn(&mut DecisionForm)>;
    let cases: Vec<(Edit, &str)> = vec![
        (Box::new(|f| f.state = "  ".into()), "state:"),
        (
            Box::new(|f| {
                f.state_mode = StateMode::Json;
                f.state = "{oops".into()
            }),
            "state: not JSON",
        ),
        (Box::new(|f| f.questions.clear()), "questions: add"),
        (
            Box::new(|f| f.questions[1].id = " ".into()),
            "question 2: give it an id",
        ),
        (
            Box::new(|f| f.questions[2].id = "team".into()),
            "question 3: the id 'team' is used twice",
        ),
        (
            Box::new(|f| {
                f.questions[0].kind = QuestionKind::Choice {
                    options: Vec::new(),
                }
            }),
            "question 'team': a choice needs at least one option",
        ),
        (
            Box::new(|f| {
                if let QuestionKind::Choice { options } = &mut f.questions[0].kind {
                    options[2].name = "billing".into();
                }
            }),
            "question 'team': the option 'billing' is offered twice",
        ),
        (
            Box::new(|f| {
                if let QuestionKind::Choice { options } = &mut f.questions[0].kind {
                    options[1].name = String::new();
                }
            }),
            "question 'team': option 2 has no name",
        ),
        (
            Box::new(|f| {
                f.questions[1].kind = QuestionKind::Score {
                    levels: vec!["one".into()],
                }
            }),
            "question 'urgency': a score has 2 to 10 levels, not 1",
        ),
    ];
    for (edit, want) in cases {
        let mut f = ticket();
        edit(&mut f);
        let got = f.check().unwrap_err();
        assert!(got.starts_with(want), "{got} (wanted {want})");
    }
    // A JSON `null` state is a state: LiquidAI's d1 types take one.
    let mut f = ticket();
    f.state_mode = StateMode::Json;
    f.state = "null".into();
    assert_eq!(f.check(), Ok(()));
}

#[test]
fn the_title_is_the_first_line_and_says_when_it_is_cut() {
    let mut f = ticket();
    assert_eq!(f.title(), "Charged \"twice\"");
    f.state = format!("\n\n  {}  \nrest", "x".repeat(TITLE_CHARS + 5));
    let t = f.title();
    assert_eq!(t.chars().count(), TITLE_CHARS + 1);
    assert!(t.ends_with('…'));
    f.state = String::new();
    assert_eq!(f.title(), "team");
}

#[test]
fn the_count_pieces_are_the_state_each_question_and_its_option_span() {
    let p = ticket().count_pieces();
    assert_eq!(p.state, "Charged \"twice\"\nplease refund");
    assert_eq!(
        p.questions[0],
        (
            "team".into(),
            "Which team?\ntechnical\nbugs\nbilling\nrefunds\nother".into(),
            Some("technical\nbugs\nbilling\nrefunds\nother".into())
        )
    );
    assert_eq!(
        p.questions[1],
        ("urgency".into(), "How urgent?\nlater\nnow".into(), None)
    );
    assert_eq!(
        p.questions[2],
        ("angry".into(), "Upset?\nupset".into(), None)
    );
}

#[test]
fn the_form_round_trips_with_its_lists_in_order() {
    let mut f = ticket();
    f.images = vec![
        ImageInput::Artifact {
            id: 4,
            name: "b.png".into(),
        },
        ImageInput::DataUrl {
            name: "a.png".into(),
            url: "data:image/png;base64,AA==".into(),
        },
    ];
    let text = serde_json::to_string(&f).unwrap();
    assert!(text.contains(r#""source":"artifact","id":4"#), "{text}");
    let back: DecisionForm = serde_json::from_str(&text).unwrap();
    assert_eq!(back, f);
}
