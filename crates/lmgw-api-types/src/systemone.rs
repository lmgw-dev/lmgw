//! TypeSafe's System One wire as llama.cpp serves it: `POST /v1/systemone`'s
//! request and answer (decision-models design §1.2, §2.8).
//!
//! lmgw relays this wire byte for byte and never re-serializes a client's
//! body or an upstream's answer through these types. They are the schema the
//! OpenAPI document is generated from (§7 Q5), what the drift tests read
//! llama.cpp's, TypeSafe's and OpenRouter's answers into, and what lmgw's own
//! callers build a question and read an answer with (§3.7, §5).
//!
//! No type denies unknown fields: an upstream's extensions pass. The ones
//! documented today are fields of their own, so an answer round-trips
//! without loss: llama.cpp's `images` and `files` on the request, and
//! OpenRouter's `id`, `provider` and `usage.cost` on the answer.
//!
//! Order is part of the wire. llama.cpp keeps a JSON object's members in the
//! order they were sent, so the order of a `choice` question's `criteria` is
//! the order of its options, and the order of `questions` is the order of the
//! answers. Every map here is an [`Ordered`], never a `BTreeMap`.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// A JSON object whose members keep the order they were written in.
///
/// Read from an object and written as one. A member named twice keeps the
/// first one's place and the last one's value, which is what a JSON reader
/// that keeps the order does with it.
#[derive(Debug, Clone, PartialEq)]
pub struct Ordered<V>(pub Vec<(String, V)>);

impl<V> Default for Ordered<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<V> Ordered<V> {
    /// The value of the member named `key`.
    pub fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<K: Into<String>, V> FromIterator<(K, V)> for Ordered<V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(iter.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
}

impl<V: Serialize> Serialize for Ordered<V> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for Ordered<V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Members<V>(PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for Members<V> {
            type Value = Ordered<V>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut out: Vec<(String, V)> = Vec::new();
                while let Some((k, v)) = a.next_entry::<String, V>()? {
                    match out.iter_mut().find(|(seen, _)| *seen == k) {
                        Some(slot) => slot.1 = v,
                        None => out.push((k, v)),
                    }
                }
                Ok(Ordered(out))
            }
        }
        d.deserialize_map(Members(PhantomData))
    }
}

/// An object of `V`s, as a map's schema: the order is the wire's, and JSON
/// Schema has no word for it.
#[cfg(feature = "schema")]
impl<V: schemars::JsonSchema> schemars::JsonSchema for Ordered<V> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("Ordered_{}", V::schema_name()).into()
    }

    fn json_schema(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "additionalProperties": g.subschema_for::<V>(),
        })
    }
}

// ---------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------

/// `POST /v1/systemone`'s body: a state and the questions to answer about it.
///
/// lmgw reads only `model` from a client's body and forwards the rest as it
/// came; the upstream validates its own wire (TypeSafe answers a malformed
/// question with 422, llama.cpp with 400).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SystemOneRequest {
    /// Required: lmgw routes by it, as TypeSafe requires it and llama-server's
    /// router mode needs it. Rewritten to the route's upstream model on the
    /// way out; nothing else is.
    pub model: String,
    /// What the questions are about: a string, or an object or array (a chat
    /// log, a record), which llama.cpp gives the model as JSON text. `null`
    /// only for the types that take none (LiquidAI's d1 models); llama.cpp
    /// refuses it on the others.
    pub state: Value,
    /// The questions, keyed by ids the caller picks; the answers come back
    /// under the same keys.
    pub questions: Ordered<Question>,
    /// llama.cpp's extension: images as data URLs, for a model started with a
    /// projector and a type that reads images (501 otherwise). An alias of
    /// `files`. TypeSafe takes text only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    /// llama.cpp's extension: files as data URLs (`images` is its alias).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
}

/// One question, by its `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "SystemOneQuestion"))]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// Pick one of the named options.
    Choice(ChoiceQuestion),
    /// Rate on 2 to 10 ordered levels.
    Score(ScoreQuestion),
    /// Yes or no.
    Noul(NoulQuestion),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChoiceQuestion {
    /// The question: a string, an object or an array.
    pub instructions: Value,
    /// Each option with its description, or `null` when it needs none, in
    /// the order the options are offered. TypeSafe takes up to 255; llama.cpp
    /// names its own per-type limit when a request crosses it.
    pub criteria: Ordered<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScoreQuestion {
    /// The question: a string, an object or an array.
    pub instructions: Value,
    /// The levels' descriptions, lowest first: 2 to 10 of them.
    pub criteria: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NoulQuestion {
    /// The yes/no question: a string, an object or an array.
    pub instructions: Value,
    /// What a yes and a no mean, when the question alone does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<NoulCriteria>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NoulCriteria {
    /// What a yes means.
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub yes: Option<String>,
    /// What a no means.
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub no: Option<String>,
}

// ---------------------------------------------------------------------------
// The answer
// ---------------------------------------------------------------------------

/// `POST /v1/systemone`'s answer, as the upstream sent it.
///
/// `model` is the upstream's own: the versioned id that answered on TypeSafe
/// (`jev-1.13.0`), a dated id on OpenRouter, the server's `--alias` on
/// llama.cpp. lmgw does not write the requested name back; which alias
/// answered is `x-lmgw-fallback`'s to say (§2.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SystemOneResponse {
    /// OpenRouter's extension: the generation's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub model: String,
    /// OpenRouter's extension: the provider that answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// One answer per question, under the question's key.
    pub answers: Ordered<Answer>,
    pub usage: Usage,
}

/// One answer, by its question's `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "SystemOneAnswer"))]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Choice(ChoiceAnswer),
    Score(ScoreAnswer),
    Noul(NoulAnswer),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChoiceAnswer {
    /// The most probable option.
    pub choice: String,
    /// Every option with its probability; they sum to 1.
    pub probabilities: Ordered<f64>,
    /// How certain the model is, derived from the probabilities.
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScoreAnswer {
    /// The expected level index, weighted by probability: it can fall between
    /// two levels.
    pub score: f64,
    /// Each level's index (`"0"`, `"1"`, …) with its description.
    pub legend: Ordered<String>,
    /// Each level's index with its probability.
    pub probabilities: Ordered<f64>,
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NoulAnswer {
    /// P(yes), from 0 (no) to 1 (yes).
    pub noul: f64,
}

/// What a request processed: lmgw logs `input_tokens` as the row's prompt
/// tokens and `output_tokens` as its completion tokens, and prices them from
/// its own price rows (§2.6).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "SystemOneUsage"))]
pub struct Usage {
    pub input_tokens: u64,
    /// Always 0 on llama.cpp; TypeSafe and OpenRouter report a non-zero
    /// count, which Jev's price does not charge.
    pub output_tokens: u64,
    /// OpenRouter's extension: what the request cost there, in USD. Relayed,
    /// never read: lmgw prices from its own rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_keeps_the_order_and_a_repeated_key_keeps_its_place() {
        let o: Ordered<u32> = serde_json::from_str(r#"{"b": 1, "a": 2, "b": 3}"#).unwrap();
        assert_eq!(o.0, vec![("b".to_string(), 3), ("a".to_string(), 2)]);
        assert_eq!(serde_json::to_string(&o).unwrap(), r#"{"b":3,"a":2}"#);
        assert!(serde_json::from_str::<Ordered<u32>>("[1]").is_err());
    }

    #[test]
    fn a_choice_question_keeps_its_options_in_order() {
        // From the text: a `Value` would sort the options before the type
        // saw them.
        let q: Question = serde_json::from_str(
            r#"{"type": "choice", "instructions": "Which team?",
                "criteria": {"technical": null, "billing": "refunds", "other": null}}"#,
        )
        .unwrap();
        let Question::Choice(c) = &q else {
            panic!("{q:?}")
        };
        assert_eq!(
            c.criteria.keys().collect::<Vec<_>>(),
            ["technical", "billing", "other"]
        );
        assert_eq!(
            serde_json::to_string(&q).unwrap(),
            r#"{"type":"choice","instructions":"Which team?","criteria":{"technical":null,"billing":"refunds","other":null}}"#
        );
    }
}
