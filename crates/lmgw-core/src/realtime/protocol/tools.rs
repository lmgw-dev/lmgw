//! Session and response tools (realtime design §2.2, §7.4): flat function
//! tools, server-side `mcp` tools (realtime-server-tools design §1), and the
//! `tool_choice` union.
//!
//! A function tool is offered to the model as it is; an `mcp` tool names a
//! label whose tools the session lists and offers (realtime-server-tools
//! §1.2, §2.1). Its rules beyond the shape — the shared `allowed_tools` /
//! `require_approval` parser, one entry per label, no approvals yet — are
//! checked where the tools arrive (`realtime::mcp_tools`).

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::mcp::{McpChoice, McpTool};

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
    /// Redacted as it is parsed ([`McpTool::redacted`]).
    Mcp(McpTool),
}

impl Tool {
    /// The `mcp` tool this is, if it is one.
    pub fn as_mcp(&self) -> Option<&McpTool> {
        match self {
            Self::Mcp(t) => Some(t),
            Self::Function { .. } => None,
        }
    }
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
    Mcp(McpTool),
}

impl<'de> Deserialize<'de> for Tool {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = super::with_default_type(Value::deserialize(d)?, "function");
        Ok(
            match TaggedTool::deserialize(v).map_err(serde::de::Error::custom)? {
                TaggedTool::Function {
                    name,
                    description,
                    parameters,
                } => Self::Function {
                    name,
                    description,
                    parameters,
                },
                TaggedTool::Mcp(t) => Self::Mcp(t.redacted()),
            },
        )
    }
}

/// `tool_choice`: a mode, one named function, or an `mcp` label's tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(ToolChoiceMode),
    Function(FunctionChoice),
    Mcp(McpChoice),
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
