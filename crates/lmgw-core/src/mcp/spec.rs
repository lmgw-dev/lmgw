//! The `{"type": "mcp"}` tool entry as a client writes it, parsed once for
//! both routes that take one: `/v1/responses` and `/v1/realtime`
//! (realtime-server-tools design §1.1). Which server or built-in toolset it
//! names, which of its tools `allowed_tools` lets through, and which
//! `require_approval` gates — so a filter means the same thing on both.
//!
//! Errors are plain messages: each route wraps them in its own error shape
//! (a 400 on `/v1/responses`, an `error` event naming the entry on
//! `/v1/realtime`).
//!
//! **`read_only` is refused, not ignored.** It filters on the server's
//! `readOnlyHint` annotation, which lmgw does not keep; dropping it would
//! widen `allowed_tools` or narrow a gate without the client knowing.

use serde_json::{Map, Value};

/// One `{"type": "mcp", ...}` entry: which registered MCP server to expose.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolSpec {
    /// Matches a registered server's tool prefix (or its name when it has no
    /// prefix). lmgw resolves labels against *its own* servers rather than
    /// dialing a `server_url`, which is the whole point of routing through it.
    pub server_label: String,
    /// `allowed_tools`, when the client narrows the server's surface. Either
    /// spelling of a tool's name matches ([`Self::allows`]).
    pub allowed_tools: Option<Vec<String>>,
    /// `require_approval` — which of this server's tools need a round trip
    /// before they run.
    pub require_approval: ApprovalRule,
}

impl McpToolSpec {
    /// Whether `allowed_tools` lets a tool through. `exposed` is the name the
    /// model sees, `upstream` the server's own name for it (a built-in's
    /// name minus its `<label>__`): a client may write either.
    pub fn allows(&self, exposed: &str, upstream: &str) -> bool {
        self.allowed_tools
            .as_ref()
            .is_none_or(|names| names.iter().any(|n| n == exposed || n == upstream))
    }
}

/// The `require_approval` field of an `mcp` tool.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ApprovalRule {
    /// `"never"` — run everything without asking. The API's default.
    #[default]
    Never,
    /// `"always"` — every call on this server is gated.
    Always,
    /// The object form, `{"never": {"tool_names": […]}, "always": {…}}`.
    Filter {
        never: Vec<String>,
        always: Vec<String>,
    },
}

impl ApprovalRule {
    /// Whether a tool needs approval. `exposed` is the prefixed name the model
    /// sees, `upstream` the server's own name for it — a client may reasonably
    /// write either in a `tool_names` list, so both match.
    ///
    /// In the object form an explicit `never` entry always wins. Past that, an
    /// `always` list is a whitelist of what to gate; if only `never` was given,
    /// it is a list of exceptions and everything else is gated — which is the
    /// reading that fails closed.
    pub fn requires(&self, exposed: &str, upstream: &str) -> bool {
        let listed = |names: &[String]| names.iter().any(|n| n == exposed || n == upstream);
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::Filter { never, always } => {
                if listed(never) {
                    false
                } else if !always.is_empty() {
                    listed(always)
                } else {
                    !never.is_empty()
                }
            }
        }
    }

    /// Whether [`Self::requires`] is false for every tool a server could
    /// have — decided without its tool list. An object form gates nothing
    /// when both lists are empty, or when every name it would gate is also
    /// one it exempts (`never` wins). Anything else may gate a tool, which is
    /// all a route without approvals needs to know (realtime-server-tools
    /// decision 4).
    pub fn gates_nothing(&self) -> bool {
        match self {
            Self::Never => true,
            Self::Always => false,
            Self::Filter { never, always } if always.is_empty() => never.is_empty(),
            Self::Filter { never, always } => always.iter().all(|a| never.contains(a)),
        }
    }
}

/// Why a `read_only` filter is refused, on both routes (§1.1).
pub const READ_ONLY_REFUSED: &str = "lmgw does not read MCP tool annotations, so `read_only` \
     cannot be honoured; list the tool names";

/// One `{"type": "mcp"}` entry of a request's `tools`.
pub fn parse_mcp_tool(t: &Value) -> Result<McpToolSpec, String> {
    let label = t
        .get("server_label")
        .and_then(Value::as_str)
        .ok_or_else(|| "mcp tool without 'server_label'".to_string())?
        .to_string();
    Ok(McpToolSpec {
        allowed_tools: parse_allowed_tools(t.get("allowed_tools"), &label)?,
        require_approval: parse_require_approval(t.get("require_approval"), &label)?,
        server_label: label,
    })
}

/// `allowed_tools` in either spelling: a list of names, or the filter object
/// `{tool_names: […]}` (which `@openai/agents` always sends). `None` lets
/// every tool through; `Some([])` lets none, and the resolver says so.
///
/// A filter object without `tool_names` filters nothing, as an absent field
/// does. Any other value is refused: read as "no filter", a typo would hand
/// the model every tool the server has. So is an entry that is not a name:
/// dropped, it would leave a different filter than the one written.
pub fn parse_allowed_tools(v: Option<&Value>, label: &str) -> Result<Option<Vec<String>>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => Ok(Some(strings(
            a,
            &format!("allowed_tools on mcp server '{label}'"),
        )?)),
        Some(Value::Object(o)) => {
            refuse_read_only(o, &format!("allowed_tools on mcp server '{label}'"))?;
            match o.get("tool_names") {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Array(a)) => Ok(Some(strings(
                    a,
                    &format!("allowed_tools.tool_names on mcp server '{label}'"),
                )?)),
                Some(other) => Err(format!(
                    "allowed_tools.tool_names on mcp server '{label}' must be an array of tool \
                     names, got {}",
                    kind_of(other)
                )),
            }
        }
        Some(other) => Err(format!(
            "allowed_tools on mcp server '{label}' must be an array of tool names or \
             {{\"tool_names\": […]}}, got {}",
            kind_of(other)
        )),
    }
}

/// `require_approval` in either of its two spellings.
pub fn parse_require_approval(v: Option<&Value>, label: &str) -> Result<ApprovalRule, String> {
    match v {
        None | Some(Value::Null) => Ok(ApprovalRule::Never),
        Some(Value::String(s)) => match s.as_str() {
            "never" => Ok(ApprovalRule::Never),
            "always" => Ok(ApprovalRule::Always),
            other => Err(format!(
                "require_approval on mcp server '{label}': unknown value '{other}' \
                 (expected \"never\", \"always\", or an object with never/always \
                 tool_names)"
            )),
        },
        Some(Value::Object(o)) => {
            let names = |key: &str| -> Result<Vec<String>, String> {
                let Some(filter) = o.get(key).and_then(Value::as_object) else {
                    return Ok(Vec::new());
                };
                refuse_read_only(
                    filter,
                    &format!("require_approval.{key} on mcp server '{label}'"),
                )?;
                match filter.get("tool_names").and_then(Value::as_array) {
                    Some(a) => strings(
                        a,
                        &format!("require_approval.{key}.tool_names on mcp server '{label}'"),
                    ),
                    None => Ok(Vec::new()),
                }
            };
            Ok(ApprovalRule::Filter {
                never: names("never")?,
                always: names("always")?,
            })
        }
        Some(other) => Err(format!(
            "require_approval on mcp server '{label}' must be a string or an object, \
             got {}",
            kind_of(other)
        )),
    }
}

fn refuse_read_only(filter: &Map<String, Value>, at: &str) -> Result<(), String> {
    match filter.get("read_only") {
        None | Some(Value::Null) => Ok(()),
        Some(_) => Err(format!("{at}: {READ_ONLY_REFUSED}")),
    }
}

/// A list of tool names, or the first entry that is not one — by its index
/// in the list `at` names.
fn strings(a: &[Value], at: &str) -> Result<Vec<String>, String> {
    a.iter()
        .enumerate()
        .map(|(n, v)| match v {
            Value::String(s) => Ok(s.clone()),
            other => Err(format!(
                "{at}: entry {n} is {} {}, not a tool name",
                article(other),
                kind_of(other)
            )),
        })
        .collect()
}

/// "a" or "an", for [`kind_of`]'s word.
fn article(v: &Value) -> &'static str {
    match v {
        Value::Array(_) | Value::Object(_) => "an",
        _ => "a",
    }
}

/// The JSON type of `v`, for a message about a value of the wrong one.
pub fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn allowed_tools_takes_both_spellings_and_matches_both_names() {
        for v in [
            json!(["query"]),
            json!({"tool_names": ["query"]}),
            json!({"tool_names": ["docs__query"]}),
        ] {
            let spec =
                parse_mcp_tool(&json!({"server_label": "docs", "allowed_tools": v})).unwrap();
            assert!(spec.allows("docs__query", "query"), "{v}");
            assert!(!spec.allows("docs__list", "list"), "{v}");
        }
        // No filter: everything; an empty one: nothing.
        for v in [json!(null), json!({}), json!({"tool_names": null})] {
            assert_eq!(parse_allowed_tools(Some(&v), "x").unwrap(), None, "{v}");
        }
        assert_eq!(
            parse_allowed_tools(Some(&json!({"tool_names": []})), "x").unwrap(),
            Some(vec![])
        );
        let e = parse_allowed_tools(Some(&json!("query")), "x").unwrap_err();
        assert!(e.contains("got string"), "{e}");
    }

    #[test]
    fn an_entry_that_is_not_a_name_is_refused_by_its_index() {
        for (field, v, at) in [
            ("allowed_tools", json!(["query", 7]), "allowed_tools on"),
            (
                "allowed_tools",
                json!({"tool_names": ["query", null]}),
                "allowed_tools.tool_names on",
            ),
            (
                "require_approval",
                json!({"always": {"tool_names": [{"name": "query"}]}}),
                "require_approval.always.tool_names on",
            ),
        ] {
            let e = parse_mcp_tool(&json!({"server_label": "x", field: v})).unwrap_err();
            assert!(e.starts_with(at), "{v}: {e}");
            assert!(e.contains("entry"), "{v}: {e}");
        }
        let e = parse_allowed_tools(Some(&json!(["a", "b", 3])), "x").unwrap_err();
        assert_eq!(
            e,
            "allowed_tools on mcp server 'x': entry 2 is a number, not a tool name"
        );
        let e = parse_allowed_tools(Some(&json!([{}])), "x").unwrap_err();
        assert!(e.ends_with("entry 0 is an object, not a tool name"), "{e}");
    }

    #[test]
    fn read_only_is_refused_wherever_a_filter_takes_it() {
        for (field, v) in [
            ("allowed_tools", json!({"read_only": true})),
            (
                "allowed_tools",
                json!({"tool_names": ["a"], "read_only": false}),
            ),
            ("require_approval", json!({"never": {"read_only": true}})),
            ("require_approval", json!({"always": {"read_only": true}})),
        ] {
            let e = parse_mcp_tool(&json!({"server_label": "x", field: v})).unwrap_err();
            assert!(e.contains(READ_ONLY_REFUSED), "{field} {v}: {e}");
            assert!(e.contains(field), "{e}");
        }
    }

    #[test]
    fn gates_nothing_is_exact_without_the_tool_list() {
        let rule = |v: Value| parse_require_approval(Some(&v), "x").unwrap();
        for v in [
            json!(null),
            json!("never"),
            json!({}),
            json!({"never": {"tool_names": []}}),
            json!({"always": {"tool_names": ["a"]}, "never": {"tool_names": ["a", "b"]}}),
        ] {
            let r = rule(v.clone());
            assert!(r.gates_nothing(), "{v}");
            assert!(!r.requires("x__a", "a") && !r.requires("x__z", "z"), "{v}");
        }
        for v in [
            json!("always"),
            json!({"never": {"tool_names": ["a"]}}),
            json!({"always": {"tool_names": ["a"]}}),
            json!({"always": {"tool_names": ["a", "b"]}, "never": {"tool_names": ["a"]}}),
        ] {
            assert!(!rule(v.clone()).gates_nothing(), "{v}");
        }
    }
}
