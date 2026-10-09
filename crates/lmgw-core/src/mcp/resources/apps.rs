//! The MCP Apps metadata a tool carries, and the URIs in a tool's result
//! (SEP-1865, 2026-01-26; the module doc of [`super`]).

use rmcp::model::{CallToolResult, ContentBlock, Meta, Tool};
use serde_json::{json, Map, Value};

use super::uri::namespaced;

/// The extension's identifier in `capabilities.extensions`.
pub const UI_EXTENSION: &str = lmgw_api_types::mcp_apps::EXTENSION;

/// A UI resource's MIME type, the one `mimeTypes` names.
pub const UI_MIME_TYPE: &str = lmgw_api_types::mcp_apps::MIME_TYPE;

/// The deprecated flat key for `_meta.ui.resourceUri` (removed before GA).
const FLAT_RESOURCE_URI: &str = "ui/resourceUri";

/// The extension's settings, as `capabilities.extensions[UI_EXTENSION]`.
pub fn ui_extension_settings() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("mimeTypes".into(), json!([UI_MIME_TYPE]));
    m
}

/// The UI resource a tool links to: `_meta.ui.resourceUri`, else the
/// deprecated `_meta["ui/resourceUri"]`.
pub fn ui_resource(meta: Option<&Meta>) -> Option<&str> {
    let meta = &meta?.0;
    meta.get("ui")
        .and_then(|ui| ui.get("resourceUri"))
        .and_then(Value::as_str)
        .or_else(|| meta.get(FLAT_RESOURCE_URI).and_then(Value::as_str))
}

/// May a model be offered `tool`? Unless its `_meta.ui.visibility` leaves
/// `"model"` out: absent, it is `["model", "app"]`. A single string is read
/// as a list of one.
pub fn model_visible(tool: &Tool) -> bool {
    let Some(v) = tool
        .meta
        .as_ref()
        .and_then(|m| m.0.get("ui"))
        .and_then(|ui| ui.get("visibility"))
    else {
        return true;
    };
    match v {
        Value::Array(items) => items.iter().any(|i| i == "model"),
        Value::String(s) => s == "model",
        _ => true,
    }
}

/// `tool` as `/mcp` lists it for a server with tool prefix `prefix`: its UI
/// resource's URI namespaced, in both spellings.
pub fn namespace_tool(prefix: &str, tool: &mut Tool) {
    if prefix.trim().is_empty() {
        return;
    }
    let Some(meta) = tool.meta.as_mut() else {
        return;
    };
    if let Some(Value::Object(ui)) = meta.0.get_mut("ui") {
        if let Some(Value::String(u)) = ui.get_mut("resourceUri") {
            *u = namespaced(prefix, u);
        }
    }
    if let Some(Value::String(u)) = meta.0.get_mut(FLAT_RESOURCE_URI) {
        *u = namespaced(prefix, u);
    }
}

/// `result` as it leaves lmgw for a server with tool prefix `prefix`: the
/// URIs of its resource links and embedded resources namespaced. Nothing
/// else in it changes (server-tools decision 7; L14 is the one departure).
pub fn namespace_result(prefix: &str, result: &mut CallToolResult) {
    if prefix.trim().is_empty() {
        return;
    }
    for block in &mut result.content {
        let Ok(mut v) = serde_json::to_value(&*block) else {
            continue;
        };
        let uri = match v.get("type").and_then(Value::as_str) {
            Some("resource_link") => v.get_mut("uri"),
            Some("resource") => v.get_mut("resource").and_then(|r| r.get_mut("uri")),
            _ => continue,
        };
        let Some(Value::String(u)) = uri else {
            continue;
        };
        *u = namespaced(prefix, u);
        if let Ok(b) = serde_json::from_value::<ContentBlock>(v) {
            *block = b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(meta: Value) -> Tool {
        serde_json::from_value(json!({
            "name": "t", "inputSchema": {"type": "object"}, "_meta": meta
        }))
        .unwrap()
    }

    #[test]
    fn visibility_leaves_a_tool_out_only_when_it_names_no_model() {
        assert!(model_visible(&tool(json!({}))));
        assert!(model_visible(&tool(
            json!({"ui": {"resourceUri": "ui://x"}})
        )));
        assert!(model_visible(&tool(
            json!({"ui": {"visibility": ["model", "app"]}})
        )));
        assert!(model_visible(&tool(
            json!({"ui": {"visibility": ["model"]}})
        )));
        assert!(!model_visible(&tool(
            json!({"ui": {"visibility": ["app"]}})
        )));
        assert!(!model_visible(&tool(json!({"ui": {"visibility": []}}))));
        assert!(!model_visible(&tool(json!({"ui": {"visibility": "app"}}))));
    }

    #[test]
    fn both_spellings_of_the_resource_uri_are_namespaced() {
        let mut t = tool(
            json!({"ui": {"resourceUri": "ui://w/card", "visibility": ["app"]},
                                "ui/resourceUri": "ui://w/card", "other": "ui://keep"}),
        );
        namespace_tool("p", &mut t);
        let meta = t.meta.as_ref().unwrap();
        assert_eq!(ui_resource(Some(meta)), Some("ui://p__w/card"));
        assert_eq!(meta.0["ui/resourceUri"], "ui://p__w/card");
        assert_eq!(meta.0["other"], "ui://keep");
        assert_eq!(meta.0["ui"]["visibility"], json!(["app"]));
    }

    #[test]
    fn a_result_s_links_and_embedded_resources_are_namespaced() {
        let mut r: CallToolResult = serde_json::from_value(json!({
            "content": [
                {"type": "text", "text": "ui://w/card"},
                {"type": "resource_link", "uri": "ui://w/card", "name": "card"},
                {"type": "resource", "resource": {"uri": "file:///a", "text": "x"}}
            ],
            "structuredContent": {"uri": "ui://w/card"}
        }))
        .unwrap();
        namespace_result("p", &mut r);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["content"][0]["text"], "ui://w/card");
        assert_eq!(v["content"][1]["uri"], "ui://p__w/card");
        assert_eq!(v["content"][2]["resource"]["uri"], "file://p__/a");
        assert_eq!(v["structuredContent"]["uri"], "ui://w/card");
    }
}
