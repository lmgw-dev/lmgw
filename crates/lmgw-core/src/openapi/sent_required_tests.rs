//! What an answer's schema promises: a field is `required` exactly where the
//! server always sends it (`schemas::apply_sent_required`). The live half of
//! the check is `round_trips` in `tests/it/common` (every `required` field of
//! a documented type is in the real answer) and `tests/it/openapi_live.rs`
//! (every GET answer validated against its own schema); this half reads the
//! document.

use std::collections::BTreeSet;

use serde_json::Value;

use super::build::admin_doc;
use super::schemas::collect_refs;

/// Every object schema with properties reachable from a response, none of
/// which lists a `required` field, and why that is right: the server leaves
/// each key out unless it has something to say. A prefix of the label
/// `walk` makes (a component name, then `/properties/<name>` steps, or
/// `<method> <path>/...` for an inline schema).
const ALL_OPTIONAL: &[(&str, &str)] = &[
    (
        "AttachmentFacts",
        "what extraction found: which keys are there depends on the attachment's kind",
    ),
    (
        "ChatCompletion/properties/usage/properties/prompt_tokens_details",
        "each counter only when the upstream reported it",
    ),
    (
        "ChatCompletionChunk/properties/usage/properties/prompt_tokens_details",
        "each counter only when the upstream reported it",
    ),
    (
        "ChatCompletionChunk/properties/choices/items/properties/delta",
        "a stream delta carries the keys that changed; the finish frame's is {}",
    ),
    (
        "Params",
        "alias-level overrides, each present only where the alias sets it",
    ),
    ("ReasoningControl", "each control present only where set"),
    (
        "SpeechCaps",
        "a capability the model's facts do not state is left out (unknown)",
    ),
    (
        "StructuredOutputCaps",
        "a capability the model's facts do not state is left out (unknown)",
    ),
    (
        "ThreadVoice",
        "a thread's own voice settings, each absent where the Chat's own apply",
    ),
    (
        "ImageGenerationsResponse/properties/data/items",
        "b64_json or url, whichever response_format asked for (an anyOf of required lists)",
    ),
    (
        "post /api/op/local_model_test/200/content/application/json/schema/anyOf/3/allOf/1",
        "the allOf arm that only pins the discriminating `class` constant of the arm's $ref",
    ),
    (
        "post /api/op/local_model_test/200/content/application/json/schema/anyOf/4/allOf/1",
        "the allOf arm that only pins the discriminating `class` constant of the arm's $ref",
    ),
];

/// `(label, schema)` for every object schema with properties under `value`.
fn walk<'a>(value: &'a Value, label: &str, out: &mut Vec<(String, &'a Value)>) {
    match value {
        Value::Object(map) => {
            if map
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|p| !p.is_empty())
            {
                out.push((label.to_string(), value));
            }
            for (k, v) in map {
                walk(v, &format!("{label}/{k}"), out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                walk(v, &format!("{label}/{i}"), out);
            }
        }
        _ => {}
    }
}

/// The component names reachable from `roots` (the operations' response or
/// request parts), `$ref`s followed.
fn reach(doc: &Value, roots: &[&Value]) -> BTreeSet<String> {
    let mut frontier = Vec::new();
    for r in roots {
        collect_refs(r, &mut frontier);
    }
    let mut seen = BTreeSet::new();
    while let Some(name) = frontier.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(schema) = doc.pointer(&format!("/components/schemas/{name}")) {
            collect_refs(schema, &mut frontier);
        }
    }
    seen
}

fn operations(doc: &Value) -> impl Iterator<Item = (String, &Value)> {
    doc["paths"].as_object().unwrap().iter().flat_map(|(p, m)| {
        m.as_object()
            .unwrap()
            .iter()
            .map(move |(method, op)| (format!("{method} {p}"), op))
    })
}

#[test]
fn an_answer_object_lists_what_the_server_always_sends() {
    let doc = admin_doc();
    let responses: Vec<&Value> = operations(doc).map(|(_, op)| &op["responses"]).collect();
    let mut found: Vec<(String, &Value)> = Vec::new();
    for name in reach(doc, &responses) {
        let schema = &doc["components"]["schemas"][name.as_str()];
        walk(schema, &name, &mut found);
    }
    for (op, o) in operations(doc) {
        walk(&o["responses"], &op, &mut found);
    }
    let mut hit = BTreeSet::new();
    let mut bad = Vec::new();
    for (label, schema) in &found {
        let listed = schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|r| !r.is_empty());
        if listed {
            continue;
        }
        match ALL_OPTIONAL.iter().find(|(p, _)| label.starts_with(p)) {
            Some((p, _)) => {
                hit.insert(*p);
            }
            None => bad.push(label.clone()),
        }
    }
    assert!(
        bad.is_empty(),
        "answer objects with no `required` (list what the server always sends: a #[serde(default)] \
         reader must not decide it; or give the shape a reason in ALL_OPTIONAL):\n{}",
        bad.join("\n")
    );
    for (p, _) in ALL_OPTIONAL {
        assert!(
            hit.contains(p),
            "ALL_OPTIONAL lists {p}, which now has a required field: remove it"
        );
    }
}

/// A component both a request and an answer reached, whose two views differ,
/// is two components: `<Name>` is the answer's, `<Name>Input` the request's.
#[test]
fn a_split_component_serves_one_side_each() {
    let doc = admin_doc();
    let responses: Vec<&Value> = operations(doc).map(|(_, op)| &op["responses"]).collect();
    let requests: Vec<Value> = operations(doc)
        .map(|(_, op)| serde_json::json!([op["requestBody"], op["parameters"]]))
        .collect();
    let requests: Vec<&Value> = requests.iter().collect();
    let sent = reach(doc, &responses);
    let read = reach(doc, &requests);
    let twins: Vec<&str> = doc["components"]["schemas"]
        .as_object()
        .unwrap()
        .keys()
        .filter_map(|n| n.strip_suffix("Input"))
        .filter(|n| doc["components"]["schemas"].get(*n).is_some())
        .collect();
    assert!(
        !twins.is_empty(),
        "the document splits at least one component"
    );
    for name in twins {
        let twin = format!("{name}Input");
        assert!(read.contains(&twin), "{twin} is reached by no request");
        assert!(!sent.contains(&twin), "{twin} is reached by an answer");
        assert!(sent.contains(name), "{name} is reached by no answer");
        assert!(!read.contains(name), "{name} is reached by a request");
    }
}
