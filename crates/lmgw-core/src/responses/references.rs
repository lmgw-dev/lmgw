//! What a native `/v1/responses` body names that the upstream holds under
//! the provider key (openai egress design §6.1, WP5b): stored items, files,
//! vector stores, containers, skills.
//!
//! The provider key is the owner's, so everything the upstream stores under
//! it is the owner's: lmgw does not proxy `/v1/files`, `/v1/vector_stores`,
//! `/v1/containers` or `/v1/skills`, so no client key ever made one of them
//! through lmgw, and naming one would reach the owner's data. A client key's
//! request that names one answers lmgw's not-found, exactly as an unknown
//! `previous_response_id` does ([`super::no_stored_response`]), so a probe
//! learns nothing about what exists. The owner key passes (ids made outside
//! lmgw on the same provider key).
//!
//! A file can be named anywhere in the body: a content part, a tool's output,
//! a container's files, an image mask, a `prompt` template's `variables`
//! (which take `input_file` and `input_image` values). So the whole body is
//! searched for `file_id` and `file_ids`, except `metadata`, which the
//! upstream only stores, and the JSON schemas a body carries as text for the
//! model — `text.format.schema` and a tool's `parameters` (`input_schema`,
//! `output_schema` where a tool has them) — whose `file_id` is a property
//! name, an example or a default, never a reference.
//!
//! Items are the exception: an input item that carries an `id` (a
//! `reasoning` item's `rs_…`, a `message`'s `msg_…`, a `*_call`'s), and an
//! `item_reference`, are resolved against what the upstream stores, so a
//! client key may name the items of answers it was given itself
//! ([`named_items`], checked against the items recorded per native answer,
//! migration 0093). A `reasoning` item that carries its `encrypted_content`
//! is checked too, its encrypted content against the hashes recorded for
//! that id (WP8 review S-R3) — every encryption the answer carried, since
//! OpenAI encrypts an item anew each time it writes it and a stream's item
//! `done` and its terminal event differ (migration 0100, api-compat design
//! §9 finding 9): OpenAI's reference says what the field holds ("The
//! encrypted content of the reasoning item",
//! <https://developers.openai.com/api/reference/resources/responses/methods/create.md>)
//! and its reasoning guide that stateless requests replay it
//! (<https://developers.openai.com/api/docs/guides/reasoning.md>), but
//! neither says that a present `id` is then not looked up, so it is not
//! assumed. A key's own `store: false` answers are recorded as well, so it
//! replays its own encrypted reasoning.

use serde_json::Value;

/// The first id in `body` a client key may not name at all: a file, a
/// vector store, a container or a skill. Items are [`named_items`]'.
pub(super) fn first_foreign(body: &Value) -> Option<String> {
    let searched = body.as_object()?;
    if let Some(id) = searched.iter().find_map(|(k, v)| match k.as_str() {
        "metadata" => None,
        "text" => text_file_ids(v),
        "tools" => v.as_array()?.iter().find_map(tool_file_ids),
        _ => file_ids(v),
    }) {
        return Some(id);
    }
    let tools = body.get("tools").and_then(Value::as_array)?;
    tools.iter().find_map(tool_reference)
}

/// The stored items `body`'s input names: each `item_reference`'s and each
/// input item's own `id` (a reasoning item's with the hash of the encrypted
/// content it carries), and for a reasoning item lmgw's loop wrote, the
/// platform's item its seal names (`super::seals`).
///
/// A whole assistant `message` or `function_call` is no reference: one whose
/// id the key was not given goes without it (`super::loop_items`), so it is
/// not checked here. Nor is a reasoning item without encrypted content: one
/// whose id the key was not given is left out of the body (WP8 verify V-F1),
/// so only the key's own goes.
pub(super) fn named_items(body: &Value) -> Vec<crate::store::ItemClaim> {
    body.get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|i| !super::loop_items::goes_as_given(i))
        .filter_map(super::seals::named_item)
        .collect()
}

/// The first string under one of `keys` anywhere under `v`, or the first
/// string of an array under one.
fn first_at(v: &Value, keys: &[&str]) -> Option<String> {
    match v {
        Value::Object(o) => {
            for k in keys {
                match o.get(*k) {
                    Some(Value::String(id)) => return Some(id.clone()),
                    Some(Value::Array(ids)) => {
                        if let Some(id) = ids.iter().find_map(Value::as_str) {
                            return Some(id.to_string());
                        }
                    }
                    _ => {}
                }
            }
            o.values().find_map(|v| first_at(v, keys))
        }
        Value::Array(a) => a.iter().find_map(|v| first_at(v, keys)),
        _ => None,
    }
}

/// The first `file_id` (or one of `file_ids`) anywhere under `v`.
fn file_ids(v: &Value) -> Option<String> {
    first_at(v, &["file_id", "file_ids"])
}

/// [`file_ids`] of everything under `entries` but the keys in `skip`.
fn file_ids_but<'a>(
    entries: impl IntoIterator<Item = (&'a String, &'a Value)>,
    skip: &[&str],
) -> Option<String> {
    let kept: serde_json::Map<String, Value> = entries
        .into_iter()
        .filter(|(k, _)| !skip.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    file_ids(&Value::Object(kept))
}

/// [`file_ids`] of the body's `text`, its `format.schema` passed over: the
/// schema of the answer the model writes (module doc).
fn text_file_ids(text: &Value) -> Option<String> {
    let Some(o) = text.as_object() else {
        return file_ids(text);
    };
    o.iter()
        .find_map(|(k, v)| match (k.as_str(), v.as_object()) {
            ("format", Some(format)) => file_ids_but(format, &["schema"]),
            _ => file_ids(v),
        })
}

/// [`file_ids`] of one tool, its schemas passed over (module doc).
fn tool_file_ids(tool: &Value) -> Option<String> {
    match tool.as_object() {
        Some(o) => file_ids_but(o, &["parameters", "input_schema", "output_schema"]),
        None => file_ids(tool),
    }
}

/// What one tool names that the upstream holds besides files: a
/// `file_search` tool's vector store, a `code_interpreter` container by id,
/// a `shell` tool's referenced container, and a skill by `skill_id` (a
/// `shell` environment's `skills`, owner-uploaded).
fn tool_reference(tool: &Value) -> Option<String> {
    if let Some(id) = tool
        .get("vector_store_ids")
        .and_then(Value::as_array)
        .and_then(|ids| ids.iter().find_map(Value::as_str))
    {
        return Some(id.to_string());
    }
    // `container` is an id string, or `{type: "auto", …}` that makes one.
    if let Some(id) = tool.get("container").and_then(Value::as_str) {
        return Some(id.to_string());
    }
    if let Some(id) = tool
        .pointer("/environment/container_id")
        .and_then(Value::as_str)
    {
        return Some(id.to_string());
    }
    first_at(tool, &["skill_id"])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn every_input_item_id_and_item_reference_is_named() {
        let body = json!({"input": [
            {"role": "user", "content": "hi"},
            {"type": "item_reference", "id": "msg_a"},
            {"id": "msg_b"},
            {"type": "reasoning", "id": "rs_1", "summary": []},
            {"type": "reasoning", "id": "rs_2", "summary": [], "encrypted_content": "gAAA"},
            {"type": "message", "id": "msg_c", "role": "assistant", "content": []},
            {"type": "web_search_call", "id": "ws_1", "status": "completed"},
            {"type": "function_call_output", "call_id": "c", "output": "x"},
        ]});
        let named = named_items(&body);
        // `msg_c`, an assistant message, is whole, and `rs_1`, without
        // encrypted content, goes only as given (`loop_items`).
        assert_eq!(
            named.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["msg_a", "msg_b", "rs_2", "ws_1"]
        );
        // The encrypted content a reasoning item carries is claimed too.
        assert_eq!(
            named[2].encrypted_sha256,
            Some(crate::ir::encrypted_sha256("gAAA"))
        );
        assert_eq!(named[1].encrypted_sha256, None);
        assert!(named_items(&json!({"input": "hi"})).is_empty());
    }

    /// A `file_id` in a JSON schema the body carries for the model — a
    /// property's example, its default — is no reference; one beside the
    /// schema still is.
    #[test]
    fn a_file_id_inside_a_schema_is_not_a_reference() {
        let schema = json!({"type": "object", "properties": {"file_id": {
            "type": "string", "examples": [{"file_id": "file-in-a-schema"}],
            "default": "file-x"}}, "examples": [{"file_id": "file-y"}]});
        let body = json!({
            "input": "hi",
            "text": {"format": {"type": "json_schema", "name": "a", "schema": schema}},
            "tools": [
                {"type": "function", "name": "f", "parameters": schema},
                {"type": "custom", "name": "g", "input_schema": schema},
            ],
        });
        assert_eq!(first_foreign(&body), None);
        let beside = json!({
            "input": "hi",
            "text": {"format": {"type": "json_schema", "name": "a", "schema": schema,
                "file_id": "file-beside"}},
        });
        assert_eq!(first_foreign(&beside).as_deref(), Some("file-beside"));
        let tool = json!({"input": "hi", "tools": [{"type": "function", "name": "f",
            "parameters": schema, "file_ids": ["file-tool"]}]});
        assert_eq!(first_foreign(&tool).as_deref(), Some("file-tool"));
    }

    #[test]
    fn files_vector_stores_containers_and_skills_are_always_foreign() {
        let part = json!({"input": [{"role": "user", "content": [
            {"type": "input_file", "file_id": "file-1"}]}]});
        assert_eq!(first_foreign(&part).as_deref(), Some("file-1"));
        let output = json!({"input": [{"type": "function_call_output", "call_id": "c",
            "output": [{"type": "input_image", "file_id": "file-2"}]}]});
        assert_eq!(first_foreign(&output).as_deref(), Some("file-2"));
        let store = json!({"input": "hi", "tools": [
            {"type": "file_search", "vector_store_ids": ["vs_1"]}]});
        assert_eq!(first_foreign(&store).as_deref(), Some("vs_1"));
        let container = json!({"input": "hi", "tools": [
            {"type": "code_interpreter", "container": "cntr_1"}]});
        assert_eq!(first_foreign(&container).as_deref(), Some("cntr_1"));
        let files = json!({"input": "hi", "tools": [
            {"type": "code_interpreter", "container": {"type": "auto", "file_ids": ["file-3"]}}]});
        assert_eq!(first_foreign(&files).as_deref(), Some("file-3"));
        let shell = json!({"input": "hi", "tools": [{"type": "shell",
            "environment": {"type": "container_reference", "container_id": "cntr_2"}}]});
        assert_eq!(first_foreign(&shell).as_deref(), Some("cntr_2"));
        let skill = json!({"input": "hi", "tools": [{"type": "shell",
            "environment": {"type": "container_auto", "skills": [
                {"type": "skill_reference", "skill_id": "skill_1"}]}}]});
        assert_eq!(first_foreign(&skill).as_deref(), Some("skill_1"));
        // A prompt template's variables take files and images.
        let prompt = json!({"prompt": {"id": "pmpt_1", "variables": {
            "doc": {"type": "input_file", "file_id": "file-4"}}}});
        assert_eq!(first_foreign(&prompt).as_deref(), Some("file-4"));
    }

    #[test]
    fn hosted_tools_that_name_nothing_stored_pass() {
        let body = json!({"input": [{"role": "user", "content": [
            {"type": "input_text", "text": "hi"},
            {"type": "input_image", "image_url": "https://img.example/a.png"},
            {"type": "input_file", "file_url": "https://docs.example/a.pdf"}]}],
            "tools": [{"type": "web_search"}, {"type": "image_generation"},
                      {"type": "code_interpreter", "container": {"type": "auto"}},
                      {"type": "function", "name": "f", "parameters": {"type": "object",
                       "properties": {"file_id": {"type": "string"}}}}],
            "metadata": {"file_id": "only-stored"}});
        assert_eq!(first_foreign(&body), None);
    }
}
