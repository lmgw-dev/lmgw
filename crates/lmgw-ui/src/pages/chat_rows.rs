//! The open thread's messages taking stored rows in place: shared by the
//! voice session's read-back (`chat_voice::realtime::reload`) and the page
//! following other writers (`chat_sync`).
//!
//! In place, so a read that changes nothing re-mounts nothing: an open
//! `<details>` stays open, the scroll stays where it is.

use leptos::prelude::*;

use super::chat::{has_tools, Msg, MsgRow};

/// Message `m` takes stored row `rows[ri]`'s fields where they differ, and
/// says whether any did (review CL-7: a field the row has none of is
/// cleared too, as an edit elsewhere clears a reply's thinking, token
/// counts and tool record):
/// - its text, reasoning, voice, token counts, model and the alias that
///   answered, the images note, its attachments;
/// - a user message's knowledge picks, and an answer's retrieval, which is
///   the stored one of the user message before it;
/// - the tool record: cleared when the row has none, made (by `make`, under
///   the page's owner) when the message shows none. Cards both have stay as
///   shown: the live ones of the page's own turn carry their durations.
pub(super) fn patch(
    m: &Msg,
    rows: &[MsgRow],
    ri: usize,
    make: Callback<Vec<MsgRow>, Vec<Msg>>,
) -> bool {
    let r = &rows[ri];
    let mut changed = false;
    let mut take = |differs: bool, set: &dyn Fn()| {
        if differs {
            set();
            changed = true;
        }
    };
    take(m.content.with_untracked(|c| *c != r.content), &|| {
        m.content.set(r.content.clone())
    });
    take(m.reasoning.with_untracked(|c| *c != r.reasoning), &|| {
        m.reasoning.set(r.reasoning.clone())
    });
    take(m.voice.with_untracked(|v| *v != r.voice), &|| {
        m.voice.set(r.voice.clone())
    });
    let tokens = r.prompt_tokens.zip(r.completion_tokens);
    take(m.tokens.get_untracked() != tokens, &|| m.tokens.set(tokens));
    take(m.model.with_untracked(|x| *x != r.model), &|| {
        m.model.set(r.model.clone())
    });
    take(
        m.answered_by.with_untracked(|x| *x != r.answered_by),
        &|| m.answered_by.set(r.answered_by.clone()),
    );
    take(
        m.images_note.with_untracked(|x| *x != r.images_note),
        &|| m.images_note.set(r.images_note.clone()),
    );
    take(
        m.attachments.with_untracked(|a| *a != r.attachments),
        &|| m.attachments.set(r.attachments.clone()),
    );
    if r.role == "user" {
        take(m.kb_refs.with_untracked(|k| *k != r.kb_refs), &|| {
            m.kb_refs.set(r.kb_refs.clone())
        });
    } else {
        let answering = rows[..ri]
            .iter()
            .rev()
            .find(|u| u.role == "user")
            .and_then(|u| u.context.clone());
        take(m.context.with_untracked(|c| *c != answering), &|| {
            m.context.set(answering.clone())
        });
    }
    let stored_tools = r.ir_messages.as_deref().is_some_and(has_tools);
    let shown_tools = m.tools.with_untracked(|t| !t.is_empty());
    if shown_tools && !stored_tools {
        m.tools.set(Vec::new());
        changed = true;
    } else if stored_tools && !shown_tools {
        let cards = make
            .run(vec![r.clone()])
            .first()
            .map(|made| made.tools.get_untracked())
            .unwrap_or_default();
        m.tools.set(cards);
        changed = true;
    }
    if m.streaming.get_untracked() {
        m.streaming.set(false);
    }
    if m.unsaved.get_untracked() {
        m.unsaved.set(false);
    }
    changed
}

/// Messages for `rows[i]`, each `i` of `append`, made by `make`: what a user
/// turn retrieved belongs to the answer after it, which is made here without
/// the turn when the turn was shown already.
pub(super) fn appended(
    make: Callback<Vec<MsgRow>, Vec<Msg>>,
    rows: &[MsgRow],
    append: &[usize],
) -> Vec<Msg> {
    let mut added = make.run(append.iter().map(|&ri| rows[ri].clone()).collect());
    for (m, &ri) in added.iter_mut().zip(append) {
        if m.role == "assistant" && m.context.with_untracked(Option::is_none) {
            if let Some(c) = rows[..ri]
                .iter()
                .rev()
                .find(|r| r.role == "user")
                .and_then(|r| r.context.clone())
            {
                m.context.set(Some(c));
            }
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::super::chat::{new_msg, Attachment, ToolCard};
    use super::super::chat_retrieval::KbContext;
    use super::*;

    /// A reply as the page shows it after its own turn: thinking, counts,
    /// the alias that answered, a note, a tool card and a retrieval.
    fn shown_reply() -> Msg {
        let m = new_msg(1, "assistant", "the old answer".into());
        m.reasoning.set("thinking".into());
        m.tokens.set(Some((10, 20)));
        m.model.set(Some("a".into()));
        m.answered_by.set(Some("b".into()));
        m.images_note.set(Some("the images were described".into()));
        m.attachments.set(vec![Attachment::default()]);
        m.context.set(Some(KbContext {
            query: "q".into(),
            ..Default::default()
        }));
        m.tools.set(vec![ToolCard {
            index: 0,
            name: RwSignal::new("lmgw__status".into()),
            args: RwSignal::new("{}".into()),
            output: RwSignal::new("ok".into()),
            is_error: RwSignal::new(false),
            ms: RwSignal::new(Some(12)),
            done: RwSignal::new(true),
        }]);
        m
    }

    fn no_make() -> Callback<Vec<MsgRow>, Vec<Msg>> {
        Callback::new(|_| panic!("nothing is made for a row without tools"))
    }

    /// Review CL-7, CL-18: another writer's edit clears a reply's thinking,
    /// counts and tool record on the server; the page clears every shown
    /// field the stored row has none of, not only the ones it has.
    #[test]
    fn a_field_the_stored_row_has_none_of_is_cleared() {
        let m = shown_reply();
        let rows = vec![
            MsgRow {
                id: 1,
                role: "user".into(),
                content: "the question".into(),
                ..Default::default()
            },
            MsgRow {
                id: 2,
                role: "assistant".into(),
                content: "the edited answer".into(),
                ..Default::default()
            },
        ];
        assert!(patch(&m, &rows, 1, no_make()));
        assert_eq!(m.content.get_untracked(), "the edited answer");
        assert_eq!(m.reasoning.get_untracked(), "");
        assert_eq!(m.tokens.get_untracked(), None);
        assert_eq!(m.model.get_untracked(), None);
        assert_eq!(m.answered_by.get_untracked(), None);
        assert_eq!(m.images_note.get_untracked(), None);
        assert!(m.attachments.with_untracked(Vec::is_empty));
        assert_eq!(
            m.context.get_untracked(),
            None,
            "the question retrieved none"
        );
        assert!(
            m.tools.with_untracked(Vec::is_empty),
            "the tool record went"
        );
        // Read again as it is now: nothing changes.
        assert!(!patch(&m, &rows, 1, no_make()));
    }

    /// The other way: a stored tool record the message does not show is
    /// made, and a retrieval of the user turn before it is taken.
    #[test]
    fn a_tool_record_and_a_retrieval_the_message_lacks_are_taken() {
        let m = new_msg(1, "assistant", "answer".into());
        let ir = serde_json::json!([
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "a", "name": "lmgw__status", "args": {}}
            ]},
            {"role": "tool", "content": [
                {"type": "tool_result", "id": "a", "is_error": false,
                 "content": [{"type": "text", "text": "ok"}]}
            ]}
        ])
        .to_string();
        let retrieved = KbContext {
            query: "q".into(),
            ..Default::default()
        };
        let rows = vec![
            MsgRow {
                id: 1,
                role: "user".into(),
                context: Some(retrieved.clone()),
                ..Default::default()
            },
            MsgRow {
                id: 2,
                role: "assistant".into(),
                content: "answer".into(),
                ir_messages: Some(ir),
                ..Default::default()
            },
        ];
        let make = Callback::new(|rows: Vec<MsgRow>| {
            rows.iter()
                .map(|_| {
                    let made = new_msg(9, "assistant", String::new());
                    made.tools.set(vec![ToolCard {
                        index: 0,
                        name: RwSignal::new("lmgw__status".into()),
                        args: RwSignal::new("{}".into()),
                        output: RwSignal::new("ok".into()),
                        is_error: RwSignal::new(false),
                        ms: RwSignal::new(None),
                        done: RwSignal::new(true),
                    }]);
                    made
                })
                .collect()
        });
        assert!(patch(&m, &rows, 1, make));
        assert_eq!(m.tools.with_untracked(Vec::len), 1);
        assert_eq!(m.context.get_untracked(), Some(retrieved));
    }
}
