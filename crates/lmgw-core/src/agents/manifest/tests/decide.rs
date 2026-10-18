//! `decide` (labs-store design §4): a `config.<field>` naming an integer
//! field of format `lmgw-question-set`, on the `enum_from` form only, and
//! the set the config names read from it.

use super::*;

/// [`batch`] with a question-set field and `decide` naming it.
fn deciding() -> Value {
    let mut v = batch();
    v["config"]["schema"]["properties"]["decision_set"] =
        json!({ "type": "integer", "format": "lmgw-question-set" });
    v["run"]["item"]["output"]["decide"] = json!("config.decision_set");
    v
}

#[test]
fn decide_names_a_question_set_field_and_reads_the_set_from_the_config() {
    let m = load_v(&deciding()).unwrap();
    let RunSpec::Batch { item, .. } = &m.run else {
        panic!("expected a batch run");
    };
    let out = item.output.as_ref().unwrap();
    assert_eq!(out.decide_field(), Some("decision_set"));
    assert_eq!(out.decide_set(&json!({ "decision_set": 7 })), Some(7));
    assert_eq!(out.decide_set(&json!({ "decision_set": null })), None);
    assert_eq!(out.decide_set(&json!({})), None, "empty: the model call");
    let fields = m.fields().unwrap();
    let f = fields.iter().find(|f| f.name == "decision_set").unwrap();
    assert_eq!(f.format, Some(Format::QuestionSet));
    assert_eq!(Format::QuestionSet.as_str(), "lmgw-question-set");
    // It survives the canonical text a row is stored as.
    assert_eq!(load(&m.to_json()).unwrap(), m);
}

#[test]
fn decide_is_refused_outside_its_shape() {
    let refused_d = |mutate: &dyn Fn(&mut Value), needle: &str| {
        let mut v = deciding();
        mutate(&mut v);
        let err = load_v(&v).expect_err("expected a refusal");
        assert!(
            err.contains(needle),
            "error did not name '{needle}':\n{err}"
        );
    };
    refused_d(
        &|v| v["run"]["item"]["output"]["decide"] = json!("decision_set"),
        "it must name a config field as config.<field>",
    );
    refused_d(
        &|v| v["run"]["item"]["output"]["decide"] = json!("config.nope"),
        "output.decide names no config field",
    );
    refused_d(
        &|v| v["run"]["item"]["output"]["decide"] = json!("config.limit"),
        "holds no question set",
    );
    // The format is an integer's: a set's id.
    refused_d(
        &|v| {
            v["config"]["schema"]["properties"]["decision_set"]["type"] = json!("string");
        },
        "format 'lmgw-question-set' applies to an integer field, not string",
    );
    // A question set answers one field: the schema form cannot decide.
    refused_d(
        &|v| {
            v["run"]["item"]["output"] = json!({
                "schema": { "type": "object", "properties": { "category": { "type": "string" } } },
                "fallback": { "category": "Other" },
                "decide": "config.decision_set"
            });
        },
        "decide belongs to the enum_from form",
    );
}

/// The shipped mail labeler decides through its `decision_set` field and
/// requires no model: with a set nothing uses one. The Run tab blocks a
/// classify with neither (the batch shape's `decide.model_field`), and the
/// field's description says Other is the review's catch-all and a failed
/// call's fallback, not an answer the set's model gives.
#[test]
fn the_shipped_mail_labeler_decides_through_its_set_and_needs_no_model() {
    let m = crate::agents::seed::shipped("mail-labeler").expect("the mail agent ships");
    assert_eq!(m.version.as_deref(), Some("3.1.0"));
    let RunSpec::Batch { item, .. } = &m.run else {
        panic!("expected a batch run");
    };
    let out = item.output.as_ref().unwrap();
    assert_eq!(out.decide_field(), Some("decision_set"));
    assert_eq!(m.model.alias, "{{config.model}}");
    let fields = m.fields().unwrap();
    let model = fields.iter().find(|f| f.name == "model").unwrap();
    assert!(!model.required, "a set classifies without a model");
    let set = fields.iter().find(|f| f.name == "decision_set").unwrap();
    assert_eq!(
        (set.ty, set.format, set.required),
        (FieldType::Integer, Some(Format::QuestionSet), false)
    );
    let says = set.description.as_deref().unwrap_or_default();
    assert!(
        says.contains("Other stays the review's catch-all and what a failed call falls back to"),
        "{says}"
    );
    assert!(says.contains("measured threshold"), "{says}");
}
