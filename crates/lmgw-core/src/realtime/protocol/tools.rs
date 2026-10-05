//! Session and response tools (realtime design §2.2, §7.4): flat function
//! tools and the `tool_choice` union.
//!
//! Only `type: "function"` is a tool here. Server-side `mcp` tools are §19,
//! and parsing them would mean accepting a tool the cascade then never offers
//! the model — so an `mcp` tool fails to parse and the client gets an `error`
//! naming the variant, rather than a session that quietly lost a tool.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// One session or response tool.
///
/// `type` is optional on input — the GA reference defaults it to
/// `"function"` — so a tool without one is a function tool rather than a
/// shape error; the echo always writes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Tool {
    Function {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// A JSON Schema, kept verbatim in the session: `@openai/agents`
        /// sends one with a `$schema` key and `anyOf` nullable unions. Only
        /// the `$schema` keys are taken out on the way to an upstream
        /// (`render`), which some providers refuse.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parameters: Option<Value>,
    },
}

/// [`Tool`] as the wire spells it once its `type` is filled in.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TaggedTool {
    Function {
        name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        parameters: Option<Value>,
    },
}

impl<'de> Deserialize<'de> for Tool {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = super::with_default_type(Value::deserialize(d)?, "function");
        let TaggedTool::Function {
            name,
            description,
            parameters,
        } = TaggedTool::deserialize(v).map_err(serde::de::Error::custom)?;
        Ok(Self::Function {
            name,
            description,
            parameters,
        })
    }
}

/// `tool_choice`: a mode, or one named function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(ToolChoiceMode),
    Function(FunctionChoice),
}

/// `"auto"` | `"none"` | `"required"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoiceMode {
    Auto,
    None,
    Required,
}

/// `{type: "function", name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionChoice {
    #[serde(rename = "type")]
    pub kind: FunctionTag,
    pub name: String,
}

/// The `"function"` literal of [`FunctionChoice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FunctionTag {
    Function,
}
