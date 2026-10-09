//! How a response's items close (realtime design §2.3, §4.3, §7.4): whole,
//! with their done events — at the end of generation, or for a spoken
//! message when its audio has played — or abandoned `incomplete` by a cancel
//! or a failure.
//!
//! **Server-side calls** (realtime-server-tools §2.2, §2.3) stay open past
//! the end of generation ([`Output::generated`]) and close with their
//! results as the responder reports them (`mcp`); an open one keeps
//! [`Output::all_closed`] false. One still open when the response ends
//! ([`Output::finish_items`]) — never run, or cut off running — closes as
//! such, and so does one a cancel abandons ([`Output::abandon`], §2.5):
//! with its `tool_execution_error`, which the next response renders.

use super::super::conversation::Conversation;
use super::super::ids::Ids;
use super::super::protocol::{ContentPart, Item, ItemStatus, ResponsePart, ServerEvent};
use super::super::writer::Outbox;
use super::Output;
use crate::ir::FinishReason;

impl Output {
    /// The model's `Stop`, for `reason` (module doc): the text and function
    /// items close — a spoken message only once its audio has played — and
    /// the server-side calls' arguments are done. A response cut off
    /// (`Length`, `ContentFilter`) runs none of its calls: they close now,
    /// never made (§2.3).
    pub fn generated(
        &mut self,
        conv: &mut Conversation,
        ids: &Ids,
        ob: &mut Outbox,
        reason: &FinishReason,
    ) {
        let status = item_status(reason);
        if self.speaks() {
            self.close_calls(conv, ob, status);
        } else {
            self.close_items(conv, ob, status);
        }
        self.mcp_generated(conv, ids, ob, status == ItemStatus::Incomplete);
    }

    /// The response ends as generated, for `reason` — its output played, or
    /// its stream failed after its end of generation: a server-side call
    /// still open closes unfinished (`mcp`), then every other item.
    pub fn finish_items(
        &mut self,
        conv: &mut Conversation,
        ids: &Ids,
        ob: &mut Outbox,
        reason: &FinishReason,
    ) {
        self.mcp_unfinished(conv, ids, ob);
        self.close_items(conv, ob, item_status(reason));
    }

    /// Close every item still open, in output order, as `status` — but a
    /// server-side call, which closes with its result (module doc).
    pub fn close_items(&mut self, conv: &mut Conversation, ob: &mut Outbox, status: ItemStatus) {
        self.text_at = None;
        for at in 0..self.items.len() as u32 {
            if !self.closed[at as usize] && !self.is_mcp(at) {
                self.close(conv, ob, at, status);
            }
        }
    }

    /// Close every function call still open, as `status` — a speaking
    /// response's at the end of generation: a call needs no audio, and the
    /// client runs its tool on the `completed` (§2.3, §7.4).
    pub fn close_calls(&mut self, conv: &mut Conversation, ob: &mut Outbox, status: ItemStatus) {
        for at in 0..self.items.len() as u32 {
            let is_call = matches!(
                conv.get(&self.items[at as usize]),
                Some(Item::FunctionCall(_))
            );
            if is_call && !self.closed[at as usize] {
                self.close(conv, ob, at, status);
            }
        }
    }

    /// Whether the response speaks: its message closes only once its audio
    /// has played, at the drained acknowledgement (§4.3, §8.2). A text
    /// response's items close as soon as generation ends.
    pub fn speaks(&self) -> bool {
        self.audio.is_some()
    }

    /// Whether every item this response announced is closed — after the
    /// end of generation in text mode (§4.3). A server-side call is open
    /// until its result is in: a cancel while it runs still cancels.
    pub fn all_closed(&self) -> bool {
        self.closed.iter().all(|c| *c)
    }

    /// A cancel or a failure: every open item stays as far as it got, marked
    /// `incomplete`, and says so with `output_item.done` and
    /// `conversation.item.done` (module doc). A spoken message first closes
    /// its audio part — `output_audio.done`, `output_audio_transcript.done`
    /// with the heard transcript, `content_part.done` — because
    /// `@openai/agents` resets its audio counters only on `output_audio.done`:
    /// without it, its next barge-in truncate is refused as "beyond the
    /// audio" (WP3 review m4).
    ///
    /// A server-side call still open closes first, by its stage
    /// (realtime-server-tools §2.5): sent as abandoned, never made as such —
    /// `output: null` and a `tool_execution_error`, with
    /// `response.mcp_call.failed` and its done events. A call that was sent
    /// before its item was announced is announced then. The responder's own
    /// reports of it come too late: they are an old generation's.
    pub fn abandon(&mut self, conv: &mut Conversation, ids: &Ids, ob: &mut Outbox) {
        self.mcp_unfinished(conv, ids, ob);
        self.text_at = None;
        for at in 0..self.items.len() {
            if std::mem::replace(&mut self.closed[at], true) {
                continue;
            }
            let item_id = self.items[at].clone();
            let Some(item) = conv.get_mut(&item_id) else {
                continue;
            };
            set_status(item, ItemStatus::Incomplete);
            let item = item.clone();
            if let Item::Message(m) = &item {
                if let Some(ContentPart::OutputAudio { transcript, .. }) = m.content.first() {
                    self.close_audio(ob, at as u32, transcript.clone().unwrap_or_default());
                }
            }
            ob.send(ServerEvent::OutputItemDone {
                response_id: self.id.clone(),
                output_index: at as u32,
                item: item.clone(),
            });
            ob.send(ServerEvent::ItemDone {
                previous_item_id: conv.previous_of(&item_id),
                item,
            });
        }
    }

    /// The closing events of one item.
    pub(super) fn close(
        &mut self,
        conv: &mut Conversation,
        ob: &mut Outbox,
        at: u32,
        status: ItemStatus,
    ) {
        self.closed[at as usize] = true;
        let item_id = self.items[at as usize].clone();
        let Some(item) = conv.get_mut(&item_id) else {
            return;
        };
        set_status(item, status);
        let item = item.clone();
        match &item {
            Item::Message(m) => match m.content.first() {
                Some(ContentPart::OutputAudio { transcript, .. }) => {
                    self.close_audio(ob, at, transcript.clone().unwrap_or_default())
                }
                other => {
                    let text = match other {
                        Some(ContentPart::OutputText { text }) => text.clone(),
                        _ => String::new(),
                    };
                    ob.send(ServerEvent::OutputTextDone {
                        at: self.part_ref(at),
                        text: text.clone(),
                    });
                    ob.send(ServerEvent::ContentPartDone {
                        at: self.part_ref(at),
                        part: ResponsePart::Text { text },
                    });
                }
            },
            Item::FunctionCall(c) => ob.send(ServerEvent::FunctionCallArgumentsDone {
                response_id: self.id.clone(),
                item_id: item_id.clone(),
                output_index: at,
                call_id: c.call_id.clone().unwrap_or_default(),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            }),
            // A response announces no listing, and its server-side calls
            // close by their own rules (`mcp`, realtime-server-tools §2.2).
            Item::FunctionCallOutput(_)
            | Item::McpCall(_)
            | Item::McpListTools(_)
            | Item::McpApprovalRequest(_)
            | Item::McpApprovalResponse(_) => {}
        }
        ob.send(ServerEvent::OutputItemDone {
            response_id: self.id.clone(),
            output_index: at,
            item: item.clone(),
        });
        ob.send(ServerEvent::ItemDone {
            previous_item_id: conv.previous_of(&item_id),
            item,
        });
    }
}

impl Output {
    /// A spoken message's audio part is over: what it said, `transcript`.
    fn close_audio(&self, ob: &mut Outbox, at: u32, transcript: String) {
        ob.send(ServerEvent::OutputAudioDone {
            at: self.part_ref(at),
        });
        ob.send(ServerEvent::OutputAudioTranscriptDone {
            at: self.part_ref(at),
            transcript: transcript.clone(),
        });
        ob.send(ServerEvent::ContentPartDone {
            at: self.part_ref(at),
            part: ResponsePart::Audio {
                audio: None,
                transcript,
            },
        });
    }
}

/// How the items of a response that stopped for `reason` end.
fn item_status(reason: &FinishReason) -> ItemStatus {
    match reason {
        FinishReason::Length | FinishReason::ContentFilter => ItemStatus::Incomplete,
        _ => ItemStatus::Completed,
    }
}

fn set_status(item: &mut Item, status: ItemStatus) {
    match item {
        Item::Message(m) => m.status = Some(status),
        Item::FunctionCall(c) => c.status = Some(status),
        Item::FunctionCallOutput(o) => o.status = Some(status),
        // No `status` on these (realtime-server-tools §2.2).
        Item::McpCall(_)
        | Item::McpListTools(_)
        | Item::McpApprovalRequest(_)
        | Item::McpApprovalResponse(_) => {}
    }
}
