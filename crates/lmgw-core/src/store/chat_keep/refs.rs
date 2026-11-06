//! A kept chat's refs to its own files (chat-media-tools design §6.3, §6.4).
//!
//! A temporary thread's attachments have ids of their own, and Keep gives
//! each a new row id. The turn record (`ir_messages`) names the pictures a
//! tool call made by those ids — `Resource{uri: "lmgw:attachment/N"}` where
//! the picture was, `att:N` in the result's text — and a tool call names its
//! input the same way (`image__edit {"image": "att:N"}`), so the record is
//! rewritten with the new ids; otherwise every ref would name a file that is
//! gone, and the model would get `[picture att:N was deleted]` back. A
//! picture's `meta.media.parent` names the picture it was made from the
//! same way ([`meta`]): a re-roll of a kept edit edits its input again.

use std::collections::HashMap;

use crate::ir::{ContentPart, Message, ToolResultBlock};
use crate::mcp::media::result::{attachment_of_uri, ATTACHMENT_URI};

/// `ir` with every ref to an attachment in `ids` (old id → new id)
/// rewritten; `None` when it names none of them, or is not a record.
pub(super) fn rewrite(ir: &str, ids: &HashMap<i64, i64>) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    let mut messages: Vec<Message> = serde_json::from_str(ir).ok()?;
    let mut changed = false;
    for m in &mut messages {
        for p in &mut m.content {
            let content = match p {
                ContentPart::ToolResult { content, .. } => content,
                ContentPart::ToolUse { args, .. } => {
                    changed |= in_args(args, ids);
                    continue;
                }
                _ => continue,
            };
            for b in content {
                match b {
                    ToolResultBlock::Resource { uri, .. } => {
                        if let Some(new) = attachment_of_uri(uri).and_then(|id| ids.get(&id)) {
                            *uri = format!("{ATTACHMENT_URI}{new}");
                            changed = true;
                        }
                    }
                    ToolResultBlock::Text { text } => {
                        if let Some(t) = in_text(text, ids) {
                            *text = t;
                            changed = true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if !changed {
        return None;
    }
    serde_json::to_string(&messages).ok()
}

/// `meta` with its `media.parent` rewritten to the kept parent's id, or
/// taken out when the parent was not kept (its old id would name nothing in
/// the stored thread); `None` when it names no parent.
pub(super) fn meta(meta: &serde_json::Value, ids: &HashMap<i64, i64>) -> Option<serde_json::Value> {
    let parent = meta.pointer("/media/parent")?.as_i64()?;
    let mut out = meta.clone();
    let media = out.get_mut("media")?.as_object_mut()?;
    match ids.get(&parent) {
        Some(new) => {
            media.insert("parent".into(), serde_json::Value::from(*new));
        }
        None => {
            media.remove("parent");
        }
    }
    Some(out)
}

/// Every `att:N` in a tool call's `args` rewritten, in each string of it
/// however deep; whether one was.
fn in_args(args: &mut serde_json::Value, ids: &HashMap<i64, i64>) -> bool {
    use serde_json::Value;
    match args {
        Value::String(s) => match in_text(s, ids) {
            Some(t) => {
                *s = t;
                true
            }
            None => false,
        },
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |changed, v| in_args(v, ids) | changed),
        Value::Object(map) => map
            .values_mut()
            .fold(false, |changed, v| in_args(v, ids) | changed),
        _ => false,
    }
}

/// `text` with every `att:N` whose `N` is in `ids` rewritten; `None` when
/// it has none.
fn in_text(text: &str, ids: &HashMap<i64, i64>) -> Option<String> {
    const REF: &str = "att:";
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut rest = text;
    while let Some(at) = rest.find(REF) {
        // A ref starts a word: `att:3`, never the tail of `flatt:3`.
        let starts_word = rest[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        out.push_str(&rest[..at + REF.len()]);
        let after = &rest[at + REF.len()..];
        let sign = usize::from(after.starts_with('-'));
        let digits = after[sign..].bytes().take_while(u8::is_ascii_digit).count();
        let end = sign + digits;
        let new = (starts_word && digits > 0)
            .then(|| after[..end].parse::<i64>().ok())
            .flatten()
            .and_then(|n| ids.get(&n));
        match new {
            Some(new) => {
                out.push_str(&new.to_string());
                changed = true;
                rest = &after[end..];
            }
            None => rest = after,
        }
    }
    out.push_str(rest);
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Role;

    fn ids() -> HashMap<i64, i64> {
        HashMap::from([(-3, 41), (-12, 42)])
    }

    #[test]
    fn a_ref_in_text_is_rewritten_as_a_whole_number() {
        let t = "Made image att:-3 (64x64), then att:-12; att:-31 and flatt:-3 stay.";
        assert_eq!(
            in_text(t, &ids()).unwrap(),
            "Made image att:41 (64x64), then att:42; att:-31 and flatt:-3 stay."
        );
        assert_eq!(in_text("no refs here, att: none", &ids()), None);
    }

    #[test]
    fn a_parent_gets_its_new_id_or_goes() {
        let m = serde_json::json!({"media": {"tool": "image__edit", "parent": -12}, "x": 1});
        assert_eq!(
            meta(&m, &ids()).unwrap(),
            serde_json::json!({"media": {"tool": "image__edit", "parent": 42}, "x": 1})
        );
        assert_eq!(
            meta(&m, &HashMap::from([(7, 8)])).unwrap(),
            serde_json::json!({"media": {"tool": "image__edit"}, "x": 1})
        );
        assert_eq!(
            meta(&serde_json::json!({"media": {"seed": 3}}), &ids()),
            None
        );
        assert_eq!(meta(&serde_json::json!({}), &ids()), None);
    }

    #[test]
    fn a_records_resources_texts_and_call_arguments_get_the_new_ids() {
        let call = Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse {
                id: "c1".into(),
                name: "image__edit".into(),
                args: serde_json::json!({
                    "image": "att:-12", "prompt": "make it blue, as att:-31 was",
                    "more": ["att:-3"], "n": 3,
                }),
            }],
        };
        let only_call = serde_json::to_string(&vec![call]).unwrap();
        let out: Vec<Message> =
            serde_json::from_str(&rewrite(&only_call, &ids()).unwrap()).unwrap();
        let ContentPart::ToolUse { args, .. } = &out[0].content[0] else {
            panic!("{out:?}");
        };
        assert_eq!(
            args,
            &serde_json::json!({
                "image": "att:42", "prompt": "make it blue, as att:-31 was",
                "more": ["att:41"], "n": 3,
            })
        );
        assert_eq!(rewrite(&only_call, &HashMap::from([(7, 8)])), None);

        let record = vec![Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                id: "c1".into(),
                name: Some("image__generate".into()),
                content: vec![
                    ToolResultBlock::text("Made image att:-3 (64x64)."),
                    ToolResultBlock::Resource {
                        uri: "lmgw:attachment/-3".into(),
                        mime: Some("image/png".into()),
                        text: None,
                    },
                ],
                is_error: false,
            }],
        }];
        let ir = serde_json::to_string(&record).unwrap();
        let out: Vec<Message> = serde_json::from_str(&rewrite(&ir, &ids()).unwrap()).unwrap();
        let ContentPart::ToolResult { content, .. } = &out[0].content[0] else {
            panic!("{out:?}");
        };
        assert_eq!(
            content[0],
            ToolResultBlock::text("Made image att:41 (64x64).")
        );
        assert_eq!(
            content[1],
            ToolResultBlock::Resource {
                uri: "lmgw:attachment/41".into(),
                mime: Some("image/png".into()),
                text: None,
            }
        );
        assert_eq!(rewrite(&ir, &HashMap::from([(7, 8)])), None);
    }
}
