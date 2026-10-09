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

    /// The rule that gates a call when `self` or `other` gates it — names
    /// compared exactly as written, so a caller that wants both spellings
    /// of a tool to count lists both. Read as sets of gated names: a rule
    /// gates a finite set (an `always` list without the names its `never`
    /// list exempts; none for `"never"`) or every name but a finite set (a
    /// never-only filter; none exempt for `"always"`), and the union of two
    /// such sets is again one of them.
    pub fn or(&self, other: &Self) -> Self {
        /// What a rule gates: `Only(names)`, or `AllBut(exempt names)`.
        enum Gated {
            Only(Vec<String>),
            AllBut(Vec<String>),
        }
        let gated = |r: &Self| match r {
            Self::Never => Gated::Only(Vec::new()),
            Self::Always => Gated::AllBut(Vec::new()),
            // `{}`: no list, nothing gated.
            Self::Filter { never, always } if never.is_empty() && always.is_empty() => {
                Gated::Only(Vec::new())
            }
            Self::Filter { never, always } if !always.is_empty() => Gated::Only(
                always
                    .iter()
                    .filter(|a| !never.contains(a))
                    .cloned()
                    .collect(),
            ),
            Self::Filter { never, .. } => Gated::AllBut(never.clone()),
        };
        let mut joined = match (gated(self), gated(other)) {
            (Gated::Only(mut a), Gated::Only(b)) => {
                a.extend(b);
                Gated::Only(a)
            }
            (Gated::Only(only), Gated::AllBut(but)) | (Gated::AllBut(but), Gated::Only(only)) => {
                Gated::AllBut(but.into_iter().filter(|n| !only.contains(n)).collect())
            }
            (Gated::AllBut(a), Gated::AllBut(b)) => {
                Gated::AllBut(a.into_iter().filter(|n| b.contains(n)).collect())
            }
        };
        let (Gated::Only(names) | Gated::AllBut(names)) = &mut joined;
        names.sort();
        names.dedup();
        match joined {
            Gated::Only(names) if names.is_empty() => Self::Never,
            Gated::Only(names) => Self::Filter {
                never: Vec::new(),
                always: names,
            },
            Gated::AllBut(names) if names.is_empty() => Self::Always,
            Gated::AllBut(names) => Self::Filter {
                never: names,
                always: Vec::new(),
            },
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

/// `require_approval` in either of its two spellings: `"never"` /
/// `"always"`, or `{always: {tool_names}, never: {tool_names}}` (either key
/// may be left out; `{}` is `"never"`). Anything else is refused, never read
/// as "gates nothing": an unknown key, a filter that is not an object, one
/// without `tool_names` or with another key.
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
            // Strict, unlike a lenient read that would take a typo for "no
            // filter" and gate nothing: a key that is not `always` or
            // `never`, a filter that is not `{tool_names: […]}`, one that
            // names no list — each is refused.
            if let Some(key) = o.keys().find(|k| *k != "always" && *k != "never") {
                return Err(format!(
                    "require_approval on mcp server '{label}': unknown key '{key}' (expected \
                     \"always\" and/or \"never\", each {{\"tool_names\": […]}})"
                ));
            }
            let names = |key: &str| -> Result<Vec<String>, String> {
                let at = format!("require_approval.{key} on mcp server '{label}'");
                let filter = match o.get(key) {
                    None | Some(Value::Null) => return Ok(Vec::new()),
                    Some(Value::Object(filter)) => filter,
                    Some(other) => {
                        return Err(format!(
                            "{at} must be {{\"tool_names\": […]}}, got {}",
                            kind_of(other)
                        ))
                    }
                };
                refuse_read_only(filter, &at)?;
                if let Some(k) = filter.keys().find(|k| *k != "tool_names") {
                    return Err(format!("{at}: unknown key '{k}' (expected \"tool_names\")"));
                }
                match filter.get("tool_names") {
                    Some(Value::Array(a)) => strings(
                        a,
                        &format!("require_approval.{key}.tool_names on mcp server '{label}'"),
                    ),
                    None | Some(Value::Null) => Err(format!(
                        "{at} names no tools: give \"tool_names\", or leave {key} out"
                    )),
                    Some(other) => Err(format!(
                        "require_approval.{key}.tool_names on mcp server '{label}' must be an \
                         array of tool names, got {}",
                        kind_of(other)
                    )),
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
    fn a_malformed_require_approval_is_refused_not_read_as_no_filter() {
        for (v, says) in [
            (
                json!({"always": ["a"]}),
                "must be {\"tool_names\": […]}, got array",
            ),
            (json!({"never": "a"}), "got string"),
            (json!({"always": {}}), "names no tools"),
            (json!({"always": {"tool_names": null}}), "names no tools"),
            (
                json!({"alwyas": {"tool_names": ["a"]}}),
                "unknown key 'alwyas'",
            ),
            (
                json!({"always": {"toolnames": ["a"]}}),
                "unknown key 'toolnames'",
            ),
            (
                json!({"always": {"tool_names": "a"}}),
                "must be an array of tool names",
            ),
        ] {
            let e = parse_require_approval(Some(&v), "x").unwrap_err();
            assert!(e.contains(says), "{v}: {e}");
            assert!(e.contains("require_approval"), "{v}: {e}");
        }
        // The documented shapes still read.
        for v in [
            json!({}),
            json!({"always": null}),
            json!({"always": {"tool_names": []}}),
            json!({"never": {"tool_names": ["a"]}, "always": {"tool_names": ["b"]}}),
        ] {
            assert!(parse_require_approval(Some(&v), "x").is_ok(), "{v}");
        }
    }

    /// `or` gates exactly what either rule gates, for every shape pair.
    #[test]
    fn or_gates_what_either_gates() {
        let rule = |v: Value| parse_require_approval(Some(&v), "x").unwrap();
        let shapes = [
            json!("never"),
            json!("always"),
            json!({"always": {"tool_names": ["a", "b"]}}),
            json!({"always": {"tool_names": ["a", "c"]}, "never": {"tool_names": ["c"]}}),
            json!({"never": {"tool_names": ["a"]}}),
            json!({"never": {"tool_names": ["a", "d"]}}),
            json!({}),
        ];
        for x in &shapes {
            for y in &shapes {
                let (a, b) = (rule(x.clone()), rule(y.clone()));
                let both = a.or(&b);
                for t in ["a", "b", "c", "d", "e"] {
                    assert_eq!(
                        both.requires(t, t),
                        a.requires(t, t) || b.requires(t, t),
                        "{x} or {y}, tool {t}: {both:?}"
                    );
                }
            }
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
