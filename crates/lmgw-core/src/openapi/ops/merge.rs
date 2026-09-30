//! The tool ↔ struct doc merge (api-docs design §4.7 "merge.rs"): where an
//! op shares a self-admin tool's code path, the tool's prose is what a reader
//! sees — the struct is the argument *contract* (finding §0.1), the tool
//! supplies the *words*.
//!
//! Three cases, all driven off [`crate::mcp::selfadmin::full_catalog`] (the
//! mode gate not applied: this document lists every op regardless of what
//! self-admin's current setting would hide from `tools/list`, exactly like
//! the tool inventory's own `full_catalog` reader):
//!
//! - [`description`] — the op's `description` if the table gave one, else the
//!   tool's.
//! - [`overlay_tool_props`] — for a `Struct` op, the tool's per-property
//!   descriptions override the struct's own doc comments, property by
//!   property; a property only the struct has keeps its own.
//! - [`tool_schema`] — for a `Tool` op (no struct of its own), the tool's
//!   `inputSchema` verbatim, since there is nothing else to document it with.

use serde_json::Value;

/// `tool`'s full-catalog entry: `{name, description, inputSchema: {...}}`.
fn tool_entry(tool: &str) -> Option<Value> {
    crate::mcp::selfadmin::full_catalog()
        .into_iter()
        .map(|(entry, _)| entry)
        .find(|entry| entry["name"] == tool)
}

/// The op's description (§4.7 "the tool description becomes the op
/// description unless one is given").
pub(crate) fn description(op_description: Option<&str>, tool: Option<&str>) -> Option<String> {
    if let Some(d) = op_description {
        return Some(d.to_string());
    }
    let entry = tool_entry(tool?).unwrap_or_else(|| {
        panic!(
            "openapi: op names tool '{}', which is not in the self-admin catalog",
            tool.unwrap()
        )
    });
    entry["description"].as_str().map(str::to_string)
}

/// `tool`'s `inputSchema`, verbatim (table A's "Tool" rows, §4.7): the
/// dispatcher reads the same `arg_*` names the tool's schema already
/// documents, so there is no struct of the op's own to prefer.
pub(crate) fn tool_schema(tool: &str) -> Value {
    tool_entry(tool).unwrap_or_else(|| {
        panic!("openapi: op names tool '{tool}', which is not in the self-admin catalog")
    })["inputSchema"]
        .clone()
}

/// Overlay `tool`'s per-property descriptions onto a struct-derived schema,
/// property by property (§4.7 merge rule). A property only the tool names is
/// never added — `tool_and_op_arguments_agree_except_listed_divergences`
/// (§7.1) is what would catch that drifting, and the pinned [`super::
/// DIVERGENCES`] list is where a real one is recorded, not a silent addition
/// here.
pub(crate) fn overlay_tool_props(schema: &mut Value, tool: &str) {
    let Some(entry) = tool_entry(tool) else {
        return;
    };
    let Some(tool_props) = entry["inputSchema"]["properties"].as_object() else {
        return;
    };
    let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    for (name, tool_prop) in tool_props {
        if let Some(desc) = tool_prop.get("description") {
            if let Some(prop) = props.get_mut(name) {
                prop["description"] = desc.clone();
            }
        }
    }
}
