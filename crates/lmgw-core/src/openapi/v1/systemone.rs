//! `POST /v1/systemone`'s example (decision-models design §2.8). Its schemas
//! are not here: they are generated from [`lmgw_api_types::systemone`], the
//! one `/v1` protocol whose types lmgw also builds and reads with (§7 Q5).

use lmgw_api_types::systemone::{
    ChoiceQuestion, NoulQuestion, Question, ScoreQuestion, SystemOneRequest,
};
use serde_json::Value;

/// Appendix A.3's ticket, one question of each type, built from the request
/// type so the example cannot drift from the schema.
///
/// The document holds the example as a `Value`, whose objects are sorted
/// (`serde_json::Map` is a `BTreeMap` in this build), so every key here sorts
/// into the order it is meant in: the questions `department`, `mood`,
/// `urgency`, and the options `billing`, `sales`, `technical`, `unsure`. On
/// the wire, the order sent is the order meant.
pub(crate) fn example() -> Value {
    let text = |s: &str| Value::String(s.to_string());
    let request = SystemOneRequest {
        model: "<pick a model>".into(),
        state: text(
            "Hi, my invoice from last month charged me twice for the Pro plan. Can you refund \
             one of the charges?",
        ),
        questions: [
            (
                "department",
                Question::Choice(ChoiceQuestion {
                    instructions: text("Which team should handle this ticket?"),
                    criteria: [
                        ("billing", Some("payments, invoices, refunds, plan charges")),
                        ("sales", Some("new purchases, upgrades, quotes")),
                        ("technical", Some("bugs, outages, errors in the product")),
                        ("unsure", None),
                    ]
                    .into_iter()
                    .map(|(k, v)| (k, v.map(String::from)))
                    .collect(),
                }),
            ),
            (
                "mood",
                Question::Noul(NoulQuestion {
                    instructions: text("The customer is upset."),
                    criteria: None,
                }),
            ),
            (
                "urgency",
                Question::Score(ScoreQuestion {
                    instructions: text("How urgent is this ticket?"),
                    criteria: ["not urgent", "normal", "urgent", "critical"]
                        .map(String::from)
                        .to_vec(),
                }),
            ),
        ]
        .into_iter()
        .collect(),
        images: None,
        files: None,
    };
    serde_json::to_value(request).unwrap_or_default()
}
