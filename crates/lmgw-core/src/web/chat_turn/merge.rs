//! Adjacent user messages become one when a turn's request is built
//! (chat-voice design §7.4).
//!
//! A thread can hold two user messages in a row: a send that failed (the
//! upstream refused, the GPU was held) keeps its user message and saves no
//! reply, and the next send adds another; a voice turn that saved no reply
//! does the same. Strict chat templates (Gemma- and Qwen-class, via
//! llama-server) and OpenAI-style upstreams refuse two user turns in a row,
//! so the thread would stay unanswerable. The request merges them instead,
//! as realtime's renderer does (realtime §7.2): the parts in order, and the
//! two texts that meet joined by a blank line. What is stored is untouched.
//!
//! A user message's knowledge-base context block (auto mode, chat-complete
//! §9.3) stays a part of its own, never joined onto a typed text. A merged
//! message keeps only the last block of its run: each block numbers its
//! excerpts from 1, so two of them would make a citation `[1]` ambiguous,
//! and the page maps citations to the latest retrieval.

use crate::ir::{ContentPart, Message, Role};

/// A request's messages, pushed in order, with adjacent user messages
/// merged.
#[derive(Default)]
pub(super) struct Messages {
    out: Vec<Message>,
    /// Where the last message holds its context block, when that message is
    /// the user's and has one.
    context_at: Option<usize>,
}

impl Messages {
    pub fn with_capacity(n: usize) -> Self {
        Self {
            out: Vec::with_capacity(n),
            context_at: None,
        }
    }

    /// Push `m`, merged into the message before it when both are the
    /// user's. `context_at`: where `m` holds its context block, if it has
    /// one.
    pub fn push(&mut self, m: Message, context_at: Option<usize>) {
        match self.out.last_mut() {
            Some(prev) if prev.role == Role::User && m.role == Role::User => {
                self.context_at = join(prev, self.context_at, m.content, context_at);
            }
            _ => {
                self.context_at = context_at.filter(|_| m.role == Role::User);
                self.out.push(m);
            }
        }
    }

    /// Push messages that never merge: a tool turn's stored record.
    pub fn extend(&mut self, ms: Vec<Message>) {
        if !ms.is_empty() {
            self.context_at = None;
        }
        self.out.extend(ms);
    }

    /// Push a late MCP task result's pair (`chat_tasks::render`): its call
    /// joins a directly preceding assistant message — one message, its
    /// parts then the call — and its result follows; neither merges with
    /// anything else. A pair that would open the conversation (the
    /// messages before it deleted) follows a user turn saying so
    /// (`render::OPENING`): Anthropic and Gemini take a call only after
    /// the user's turn.
    pub fn extend_joined(&mut self, ms: Vec<Message>) {
        let mut ms = ms.into_iter();
        let Some(first) = ms.next() else {
            return;
        };
        if self.out.iter().all(|m| m.role == Role::System) {
            self.out.push(Message::text(
                Role::User,
                crate::web::chat_tasks::render::OPENING.to_string(),
            ));
        }
        self.context_at = None;
        match self.out.last_mut() {
            Some(prev) if prev.role == Role::Assistant && first.role == Role::Assistant => {
                prev.content.extend(first.content);
            }
            _ => self.out.push(first),
        }
        self.out.extend(ms);
    }

    pub fn into_vec(self) -> Vec<Message> {
        self.out
    }
}

/// Append `parts` (its context block at `ctx`) to `prev` (its block at
/// `prev_ctx`): a later block replaces an earlier one, and where a text
/// meets a text, other than a block, one text, joined by a blank line (no
/// separator next to an empty one). Where the merged message holds its
/// block.
fn join(
    prev: &mut Message,
    mut prev_ctx: Option<usize>,
    mut parts: Vec<ContentPart>,
    mut ctx: Option<usize>,
) -> Option<usize> {
    if ctx.is_some() {
        if let Some(old) = prev_ctx.take() {
            prev.content.remove(old);
        }
    }
    let typed_before = matches!(prev.content.last(), Some(ContentPart::Text { .. }))
        && prev_ctx != Some(prev.content.len() - 1);
    let typed_after = matches!(parts.first(), Some(ContentPart::Text { .. })) && ctx != Some(0);
    if typed_before && typed_after {
        if let (Some(ContentPart::Text { text: before }), ContentPart::Text { text: after }) =
            (prev.content.last_mut(), parts.remove(0))
        {
            if !before.is_empty() && !after.is_empty() {
                before.push_str("\n\n");
            }
            before.push_str(&after);
        }
        ctx = ctx.map(|c| c - 1);
    }
    let base = prev.content.len();
    prev.content.extend(parts);
    ctx.map(|c| base + c).or(prev_ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> ContentPart {
        ContentPart::Image {
            mime: "image/png".into(),
            source: crate::ir::ImageSource::Base64 {
                data: "AAAA".into(),
            },
        }
    }

    fn built(msgs: Vec<Message>) -> Vec<Message> {
        let mut out = Messages::default();
        for m in msgs {
            out.push(m, None);
        }
        out.into_vec()
    }

    fn context(n: &str) -> ContentPart {
        ContentPart::text(format!("<context source=\"knowledge\">{n}</context>"))
    }

    /// A user message with an optional context block before its text.
    fn asked(ctx: Option<&str>, text: &str) -> (Message, Option<usize>) {
        let mut content = Vec::new();
        let at = ctx.map(|c| {
            content.push(context(c));
            0
        });
        content.push(ContentPart::text(text));
        (
            Message {
                role: Role::User,
                content,
            },
            at,
        )
    }

    fn merged(msgs: Vec<(Message, Option<usize>)>) -> Vec<ContentPart> {
        let mut out = Messages::default();
        for (m, at) in msgs {
            out.push(m, at);
        }
        let out = out.into_vec();
        assert_eq!(out.len(), 1);
        out.into_iter().next().unwrap().content
    }

    #[test]
    fn a_later_context_block_replaces_an_earlier_one_and_stays_its_own_part() {
        assert_eq!(
            merged(vec![
                asked(Some("first"), "first try"),
                asked(Some("second"), "second try"),
            ]),
            vec![
                ContentPart::text("first try"),
                context("second"),
                ContentPart::text("second try"),
            ]
        );
        // Three in a row: still one block, the last.
        assert_eq!(
            merged(vec![
                asked(Some("1"), "a"),
                asked(None, "b"),
                asked(Some("3"), "c"),
            ]),
            vec![
                ContentPart::text("a\n\nb"),
                context("3"),
                ContentPart::text("c"),
            ]
        );
    }

    #[test]
    fn a_context_block_is_kept_when_the_later_message_has_none() {
        assert_eq!(
            merged(vec![asked(Some("only"), "a"), asked(None, "b")]),
            vec![context("only"), ContentPart::text("a\n\nb")]
        );
        // A block that ends its message (attachments, no typed text) is
        // never joined onto.
        let first = Message {
            role: Role::User,
            content: vec![image(), context("x")],
        };
        assert_eq!(
            merged(vec![(first, Some(1)), asked(None, "b")]),
            vec![image(), context("x"), ContentPart::text("b")]
        );
    }

    #[test]
    fn two_user_texts_become_one_joined_by_a_blank_line() {
        let out = built(vec![
            Message::text(Role::System, "be brief"),
            Message::text(Role::User, "first try"),
            Message::text(Role::User, "second try"),
        ]);
        assert_eq!(
            out,
            vec![
                Message::text(Role::System, "be brief"),
                Message::text(Role::User, "first try\n\nsecond try"),
            ]
        );
    }

    #[test]
    fn parts_keep_their_order_and_only_meeting_texts_join() {
        let a = Message {
            role: Role::User,
            content: vec![image(), ContentPart::text("look")],
        };
        let b = Message {
            role: Role::User,
            content: vec![ContentPart::text("and this"), image()],
        };
        let c = Message {
            role: Role::User,
            content: vec![image(), ContentPart::text("last")],
        };
        let out = built(vec![a, b, c]);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].content,
            vec![
                image(),
                ContentPart::text("look\n\nand this"),
                image(),
                image(),
                ContentPart::text("last"),
            ]
        );
    }

    #[test]
    fn an_empty_text_adds_no_separator() {
        let out = built(vec![
            Message::text(Role::User, ""),
            Message::text(Role::User, "hello"),
        ]);
        assert_eq!(out, vec![Message::text(Role::User, "hello")]);
    }

    #[test]
    fn other_roles_are_left_alone() {
        let msgs = vec![
            Message::text(Role::User, "q"),
            Message::text(Role::Assistant, "a"),
            Message::text(Role::Assistant, "b"),
            Message::text(Role::User, "q2"),
        ];
        assert_eq!(built(msgs.clone()), msgs);
    }
}
