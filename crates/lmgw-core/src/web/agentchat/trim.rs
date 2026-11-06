//! The tool turn's stored IR without its final answer (`chat_messages.
//! ir_messages`): `content` already holds the answer, so the record must not
//! repeat it, or a reopened thread shows it, and the next turn replays it,
//! twice.

use crate::ir::{ContentPart, Message, Role};
use crate::store::ReplySeal;

/// The stored IR must not repeat the final answer that `content` already
/// holds. Only the trailing answer is dropped: the text and refusal parts
/// that end the last assistant message (both of which the Chat's `content`
/// holds as text, openai egress design §7.2.3). What stands before them
/// stays where it stood: the assistant's tool calls and their results, which
/// `content` cannot represent and this column is for, and a reasoning item
/// answered between two messages of the last turn (`[reasoning, text,
/// reasoning, text]` keeps `[reasoning, text, reasoning]`), so the replay
/// puts each item ahead of the text that followed it (WP8 review W-N4).
pub(super) fn trim_trailing_text(turn: &[Message], text: &str) -> Vec<Message> {
    let mut out = turn.to_vec();
    if text.is_empty() {
        return out;
    }
    if let Some(last) = out.last_mut() {
        // A turn that ends on calls (a gated stop's, waiting for their
        // decision) has no final answer: its text leads those calls, and
        // stays with them.
        let calls = last
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ToolUse { .. }));
        if last.role == Role::Assistant && !calls {
            while matches!(
                last.content.last(),
                Some(ContentPart::Text { .. } | ContentPart::Refusal { .. })
            ) {
                last.content.pop();
            }
            if last.content.is_empty() {
                out.pop();
            }
        }
    }
    out
}

/// A trimmed record as it is saved: the record, or none for one that holds
/// nothing a plain reply cannot (no call, no result) — a turn that ran no
/// tool replays like a plain one — with its reasoning seals then for the
/// reply's own column (openai egress design §7.3.2, §7.3.4: `ir_messages`
/// means a tool turn to the replay and the UI). Each seal is placed at the
/// length of the text the record holds ahead of it, less `prefix`, the
/// continued reply's text a continue's record starts with
/// (`prefix_record`): an offset into this turn's own text, as
/// [`ReplySeal::at`] counts it.
///
/// A resumed turn (`keep`) keeps its record whatever it holds: the reply's
/// record is the prior turn's, which this one's follows, so its seals stay
/// in place there.
pub(super) fn record_or_seals(
    record: Vec<Message>,
    prefix: usize,
    keep: bool,
) -> (Option<Vec<Message>>, Vec<ReplySeal>) {
    let tool_parts = record.iter().flat_map(|m| &m.content).any(|p| {
        matches!(
            p,
            ContentPart::ToolUse { .. } | ContentPart::ToolResult { .. }
        )
    });
    if tool_parts || (keep && !record.is_empty()) {
        return (Some(record), Vec::new());
    }
    let mut said = 0usize;
    let mut seals = Vec::new();
    for p in record
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| &m.content)
    {
        if let Some(t) = p.answer_text() {
            said += t.len();
        }
        if let Some(seal) = p.seal() {
            seals.push(ReplySeal::new(said.saturating_sub(prefix), seal.clone()));
        }
    }
    (None, seals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Seal;
    use serde_json::json;

    fn assistant(content: Vec<ContentPart>) -> Message {
        Message {
            role: Role::Assistant,
            content,
        }
    }

    /// A turn that ends on the model's refusal keeps it in `content` only:
    /// the record loses it as it loses a final text.
    #[test]
    fn a_final_refusal_is_trimmed_like_a_final_text() {
        let call = ContentPart::ToolUse {
            id: "call_1".into(),
            name: "f".into(),
            args: json!({}),
        };
        let turn = vec![
            assistant(vec![call.clone()]),
            Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    id: "call_1".into(),
                    name: None,
                    content: crate::ir::ToolResultBlock::one("ok"),
                    is_error: false,
                }],
            },
            assistant(vec![
                ContentPart::reasoning("thinking"),
                ContentPart::Refusal {
                    text: "I can't help with that.".into(),
                },
            ]),
        ];
        let out = trim_trailing_text(&turn, "I can't help with that.");
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].content, [ContentPart::reasoning("thinking")]);
        // Text and refusal alone: the trailing message goes.
        let mut turn = turn;
        turn[2] = assistant(vec![
            ContentPart::text("Partly: "),
            ContentPart::Refusal { text: "no.".into() },
        ]);
        assert_eq!(trim_trailing_text(&turn, "Partly: no.").len(), 2);
        // A turn that ends on calls keeps everything.
        turn[2] = assistant(vec![ContentPart::Refusal { text: "no.".into() }, call]);
        assert_eq!(trim_trailing_text(&turn, "no.").len(), 3);
    }

    /// A record of a turn that ran no tool is none: its seals go to the
    /// reply's column, each where its item stood in the reply's text, and
    /// it replays as a plain reply. A resumed turn's stays a record.
    #[test]
    fn a_record_without_a_tool_is_none_and_keeps_its_seals_in_place() {
        let (s1, s2) = (
            Seal::openai(&crate::ir::SealScope::new(2, "cfg"), "rs_1", "e1"),
            Seal::openai(&crate::ir::SealScope::new(2, "cfg"), "rs_2", "e2"),
        );
        let turn = vec![assistant(vec![
            ContentPart::sealed_reasoning("t", s1.clone()),
            ContentPart::text("Hm. "),
            ContentPart::sealed_reasoning("", s2.clone()),
            ContentPart::text("Done."),
        ])];
        let trimmed = trim_trailing_text(&turn, "Hm. Done.");
        assert_eq!(trimmed[0].content.len(), 3, "only the trailing text goes");
        assert_eq!(
            record_or_seals(trimmed.clone(), 0, false),
            (
                None,
                vec![ReplySeal::new(0, s1.clone()), ReplySeal::new(4, s2.clone())]
            )
        );
        // A continue's record starts with the continued text (`prefix`).
        let mut continued = trimmed.clone();
        continued[0]
            .content
            .insert(1, ContentPart::text("Before: "));
        assert_eq!(
            record_or_seals(continued, 8, false).1,
            vec![ReplySeal::new(0, s1.clone()), ReplySeal::new(4, s2.clone())]
        );
        assert_eq!(record_or_seals(Vec::new(), 0, false), (None, Vec::new()));
        assert_eq!(
            record_or_seals(trimmed.clone(), 0, true),
            (Some(trimmed), Vec::new())
        );
        let tool = vec![assistant(vec![
            ContentPart::sealed_reasoning("t", s1),
            ContentPart::ToolUse {
                id: "c".into(),
                name: "f".into(),
                args: json!({}),
            },
        ])];
        let (record, seals) = record_or_seals(tool.clone(), 0, false);
        assert_eq!((record, seals), (Some(tool), Vec::new()));
    }
}
