//! Structured output as one IR field (structured-output design §4): what the
//! client asked the answer to look like, read by every ingress (§5) and
//! written by every egress in its own dialect (§6).

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::error::GatewayError;

/// The format the client asked for. `None` on the request means it asked for
/// nothing.
///
/// Every `extra` is the keys of the client's object lmgw does not model (a
/// `null` where a modelled key stands included), kept verbatim: the
/// OpenAI-shaped egresses write them back, so an OpenAI or llama.cpp client's
/// object reaches an upstream of its own dialect as it was sent (design
/// §11.17). Anthropic and Gemini have no place for them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Plain text, stated explicitly (`{"type": "text"}`).
    Text {
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        extra: Map<String, Value>,
    },
    /// Any JSON object (OpenAI's JSON mode).
    JsonObject {
        /// llama.cpp's extension: an optional schema on `json_object`, which
        /// llama-server turns into a grammar (design §11.2).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<Value>,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        extra: Map<String, Value>,
    },
    /// Output constrained to a JSON Schema.
    JsonSchema(JsonSchemaFormat),
    /// A `type` lmgw does not model, kept verbatim: an OpenAI-shaped upstream
    /// gets it as sent, every other egress refuses it (design §11.3).
    Other { raw: Value },
}

/// OpenAI's `json_schema` object, every field as optional as the client may
/// leave it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JsonSchemaFormat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// The `json_schema` object's keys lmgw does not model, verbatim
    /// ([`ResponseFormat`]'s `extra`).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
    /// The `response_format` object's keys beside `type` and `json_schema`,
    /// verbatim.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub format_extra: Map<String, Value>,
    /// The client sent `type: json_schema` with no `json_schema` object
    /// (llama-server reads that as any object): none is written back, where
    /// OpenAI's spelling would need one (design §11.17).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bare: bool,
}

/// Which structured-output refusals an upstream's error may be read as, for
/// what the request asked (design §11.8): the schema texts only where a JSON
/// type was asked — llama.cpp's converter refuses tool-parameter schemas in
/// the same words — and the "type unsupported" texts wherever any type other
/// than `text` was.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FormatAsk {
    /// No format, or `text`: nothing is read as a structured-output refusal.
    #[default]
    Nothing,
    /// A type lmgw does not model ([`ResponseFormat::Other`]): a refusal of
    /// the type itself.
    Type,
    /// `json_schema` or `json_object`: both kinds of refusal.
    Json,
}

impl FormatAsk {
    /// What a raw `type` asks: `text` or none nothing, a JSON type `Json`,
    /// any other `Type`.
    pub fn of_type(t: Option<&str>) -> Self {
        match t {
            None | Some("text") => Self::Nothing,
            Some(t) if FormatKind::of_type(t).is_some() => Self::Json,
            Some(_) => Self::Type,
        }
    }

    /// Whether `e`, a classifier's reading of an upstream's error, may stand
    /// for this request.
    pub fn reads(self, e: &GatewayError) -> bool {
        match e {
            GatewayError::InvalidJsonSchema { .. } => self == Self::Json,
            GatewayError::ResponseFormatUnsupported { .. } => self != Self::Nothing,
            _ => false,
        }
    }
}

/// The type a capability flag answers for (`structured_output.json_schema`,
/// `.json_object`; design §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatKind {
    JsonSchema,
    JsonObject,
}

impl FormatKind {
    /// The wire name, as `response_format.type` and the flag spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JsonSchema => "json_schema",
            Self::JsonObject => "json_object",
        }
    }

    /// The kind a raw `type` names, `None` for any other type.
    pub fn of_type(t: &str) -> Option<Self> {
        match t {
            "json_schema" => Some(Self::JsonSchema),
            "json_object" => Some(Self::JsonObject),
            _ => None,
        }
    }
}

/// The name the IR gives a schema whose dialect has none (Anthropic's
/// format, llama.cpp's top-level `json_schema`): OpenAI requires one, and
/// `/v1/responses` already used this default (design §11.4).
pub const DEFAULT_SCHEMA_NAME: &str = "response";

impl ResponseFormat {
    /// The type a flag speaks for: none for `Text` and `Other`, so neither is
    /// ever refused early.
    pub fn kind(&self) -> Option<FormatKind> {
        match self {
            Self::JsonSchema(_) => Some(FormatKind::JsonSchema),
            Self::JsonObject { .. } => Some(FormatKind::JsonObject),
            Self::Text { .. } | Self::Other { .. } => None,
        }
    }

    /// Plain text with nothing else in the object.
    pub fn text() -> Self {
        Self::Text { extra: Map::new() }
    }

    /// The `type` as the client spelled it.
    pub fn type_name(&self) -> &str {
        match self {
            Self::Text { .. } => "text",
            Self::JsonObject { .. } => "json_object",
            Self::JsonSchema(_) => "json_schema",
            Self::Other { raw } => raw.get("type").and_then(Value::as_str).unwrap_or("?"),
        }
    }

    /// Which refusals an upstream's error may be read as ([`FormatAsk`]).
    pub fn ask(&self) -> FormatAsk {
        match self {
            Self::Text { .. } => FormatAsk::Nothing,
            Self::Other { .. } => FormatAsk::Type,
            Self::JsonObject { .. } | Self::JsonSchema(_) => FormatAsk::Json,
        }
    }

    /// OpenAI's chat `response_format` (design §6): exactly the fields the IR
    /// has and every key it kept verbatim, so an OpenAI client's object comes
    /// out as it went in.
    pub fn to_openai(&self) -> Value {
        match self {
            Self::Text { extra } => with_type("text", extra.clone()),
            Self::JsonObject { schema, extra } => {
                let mut o = extra.clone();
                if let Some(s) = schema {
                    o.insert("schema".into(), s.clone());
                }
                with_type("json_object", o)
            }
            Self::JsonSchema(f) => {
                let mut o = f.format_extra.clone();
                if !f.bare {
                    let mut inner = f.extra.clone();
                    if let Some(v) = &f.name {
                        inner.insert("name".into(), json!(v));
                    }
                    if let Some(v) = &f.description {
                        inner.insert("description".into(), json!(v));
                    }
                    if let Some(v) = &f.schema {
                        inner.insert("schema".into(), v.clone());
                    }
                    if let Some(v) = f.strict {
                        inner.insert("strict".into(), json!(v));
                    }
                    o.insert("json_schema".into(), Value::Object(inner));
                }
                with_type("json_schema", o)
            }
            Self::Other { raw } => raw.clone(),
        }
    }

    /// Read OpenAI's chat `response_format` (design §5). `None` for an absent
    /// or `null` field.
    pub fn from_openai(v: Option<&Value>) -> Result<Option<Self>, GatewayError> {
        let v = match v {
            None | Some(Value::Null) => return Ok(None),
            Some(v) => v,
        };
        let o = v.as_object().ok_or_else(|| {
            GatewayError::BadRequest("'response_format' must be an object".into())
        })?;
        let t = o.get("type").and_then(Value::as_str).ok_or_else(|| {
            GatewayError::BadRequest("'response_format' has no string 'type'".into())
        })?;
        Ok(Some(match t {
            "text" => Self::Text {
                extra: unmodelled(o, &["type"]),
            },
            "json_object" => Self::JsonObject {
                schema: o.get("schema").filter(|s| !s.is_null()).cloned(),
                extra: unmodelled(o, &["type", "schema"]),
            },
            "json_schema" => {
                let (inner, bare) = match o.get("json_schema") {
                    None | Some(Value::Null) => (Map::new(), true),
                    Some(Value::Object(m)) => (m.clone(), false),
                    Some(_) => {
                        return Err(GatewayError::BadRequest(
                            "'response_format.json_schema' must be an object".into(),
                        ))
                    }
                };
                let mut f = schema_fields(&inner, "response_format.json_schema")?;
                f.extra = unmodelled(&inner, &["name", "description", "schema", "strict"]);
                f.format_extra = unmodelled(o, &["type", "json_schema"]);
                f.bare = bare;
                Self::JsonSchema(f)
            }
            _ => Self::Other { raw: v.clone() },
        }))
    }
}

/// `{"type": t}` over `o`.
fn with_type(t: &str, mut o: Map<String, Value>) -> Value {
    o.insert("type".into(), json!(t));
    Value::Object(o)
}

/// The keys of `o` lmgw does not model: every one not in `modelled`, and a
/// modelled one whose value is `null` (it was not read, so it goes back as
/// it came).
fn unmodelled(o: &Map<String, Value>, modelled: &[&str]) -> Map<String, Value> {
    o.iter()
        .filter(|(k, v)| !modelled.contains(&k.as_str()) || v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// The `response_format` a raw OpenAI-shaped body states: none for a `null`
/// one, and none for a `text` one beside llama.cpp's top-level
/// `json_schema`, which llama-server then applies (design §11.18).
fn stated_format(body: &Value) -> Option<&Value> {
    let f = body.get("response_format").filter(|f| !f.is_null())?;
    let text = f.get("type").and_then(Value::as_str) == Some("text");
    let schema = body.get("json_schema").is_some_and(|s| !s.is_null());
    (!(text && schema)).then_some(f)
}

/// The JSON type a raw OpenAI-shaped body asks for, for a route that forwards
/// the body as it came (`/v1/completions`; structured-output design §7):
/// `response_format.type`, else llama.cpp's top-level `json_schema`.
pub fn body_kind(body: &Value) -> Option<FormatKind> {
    match stated_format(body) {
        Some(f) => f
            .get("type")
            .and_then(Value::as_str)
            .and_then(FormatKind::of_type),
        None => body
            .get("json_schema")
            .filter(|s| !s.is_null())
            .map(|_| FormatKind::JsonSchema),
    }
}

/// [`FormatAsk`] for a raw OpenAI-shaped body, as [`body_kind`] reads it.
pub fn body_ask(body: &Value) -> FormatAsk {
    match stated_format(body) {
        Some(f) => FormatAsk::of_type(f.get("type").and_then(Value::as_str)),
        None if body_kind(body).is_some() => FormatAsk::Json,
        None => FormatAsk::Nothing,
    }
}

/// `name`, `description`, `schema` and `strict` from an object that holds
/// them — OpenAI's `json_schema` wrapper, or a Responses `text.format`.
/// `at` names the object in a refusal.
pub(crate) fn schema_fields(
    o: &Map<String, Value>,
    at: &str,
) -> Result<JsonSchemaFormat, GatewayError> {
    let string = |k: &str| -> Result<Option<String>, GatewayError> {
        match o.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(GatewayError::BadRequest(format!(
                "'{at}.{k}' must be a string"
            ))),
        }
    };
    let strict = match o.get("strict") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => {
            return Err(GatewayError::BadRequest(format!(
                "'{at}.strict' must be a boolean"
            )))
        }
    };
    Ok(JsonSchemaFormat {
        name: string("name")?,
        description: string("description")?,
        schema: o.get("schema").filter(|s| !s.is_null()).cloned(),
        strict,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_openai_object_comes_out_as_it_went_in() {
        for v in [
            json!({"type": "text"}),
            json!({"type": "json_object"}),
            json!({"type": "json_object", "schema": {"type": "object"}}),
            json!({"type": "json_schema", "json_schema": {
                "name": "answer", "description": "d", "strict": true,
                "schema": {"type": "object"},
            }}),
            json!({"type": "json_schema", "json_schema": {"schema": {"type": "object"}}}),
            json!({"type": "structural_tag", "structures": []}),
            // Keys lmgw does not model, a null where a modelled one stands,
            // and no wrapper at all (§11.17).
            json!({"type": "text", "x-hint": 1}),
            json!({"type": "json_object", "schema": null, "x-hint": [1]}),
            json!({"type": "json_schema", "x-hint": true, "json_schema": {
                "name": "answer", "strict": null, "x-vendor": {"a": 1},
                "schema": {"type": "object"},
            }}),
            json!({"type": "json_schema"}),
            json!({"type": "json_schema", "json_schema": null}),
            json!({"type": "json_schema", "json_schema": {}}),
        ] {
            let f = ResponseFormat::from_openai(Some(&v)).unwrap().unwrap();
            assert_eq!(f.to_openai(), v);
        }
    }

    #[test]
    fn only_the_json_types_have_a_kind() {
        let kind = |v: Value| {
            ResponseFormat::from_openai(Some(&v))
                .unwrap()
                .unwrap()
                .kind()
        };
        assert_eq!(kind(json!({"type": "text"})), None);
        assert_eq!(
            kind(json!({"type": "json_object"})),
            Some(FormatKind::JsonObject)
        );
        assert_eq!(
            kind(json!({"type": "json_schema", "json_schema": {}})),
            Some(FormatKind::JsonSchema)
        );
        assert_eq!(kind(json!({"type": "regex", "pattern": "a+"})), None);
    }

    /// The schema texts only for a JSON type; the type texts for any type
    /// but `text` (§11.8).
    #[test]
    fn an_ask_reads_what_its_type_can_be_refused_for() {
        let schema = GatewayError::InvalidJsonSchema {
            message: "x".into(),
        };
        let unsupported = GatewayError::ResponseFormatUnsupported {
            message: "x".into(),
        };
        let other = GatewayError::BadRequest("x".into());
        let ask = |t| FormatAsk::of_type(t);
        for (t, schema_read, type_read) in [
            (None, false, false),
            (Some("text"), false, false),
            (Some("regex"), false, true),
            (Some("json_object"), true, true),
            (Some("json_schema"), true, true),
        ] {
            assert_eq!(ask(t).reads(&schema), schema_read, "{t:?}");
            assert_eq!(ask(t).reads(&unsupported), type_read, "{t:?}");
            assert!(!ask(t).reads(&other));
        }
    }

    /// A `text` type next to llama.cpp's top-level `json_schema` is the
    /// schema (§11.18); the body's other spellings as before.
    #[test]
    fn a_raw_body_asks_what_llama_cpp_would_apply() {
        let schema = json!({"type": "object"});
        for (body, kind, ask) in [
            (json!({}), None, FormatAsk::Nothing),
            (
                json!({"response_format": {"type": "text"}}),
                None,
                FormatAsk::Nothing,
            ),
            (
                json!({"response_format": {"type": "text"}, "json_schema": schema}),
                Some(FormatKind::JsonSchema),
                FormatAsk::Json,
            ),
            (
                json!({"response_format": null, "json_schema": schema}),
                Some(FormatKind::JsonSchema),
                FormatAsk::Json,
            ),
            (
                json!({"response_format": {"type": "json_object"}, "json_schema": schema}),
                Some(FormatKind::JsonObject),
                FormatAsk::Json,
            ),
            (
                json!({"response_format": {"type": "regex"}}),
                None,
                FormatAsk::Type,
            ),
        ] {
            assert_eq!(body_kind(&body), kind, "{body}");
            assert_eq!(body_ask(&body), ask, "{body}");
        }
    }

    #[test]
    fn a_shape_with_nothing_to_translate_is_a_bad_request() {
        for v in [
            json!("json_object"),
            json!({"json_schema": {}}),
            json!({"type": "json_schema", "json_schema": "x"}),
            json!({"type": "json_schema", "json_schema": {"strict": "yes"}}),
        ] {
            let e = ResponseFormat::from_openai(Some(&v)).unwrap_err();
            assert_eq!(e.kind(), "bad_request", "{v}");
        }
        assert_eq!(
            ResponseFormat::from_openai(Some(&Value::Null)).unwrap(),
            None
        );
    }
}
