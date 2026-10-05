//! One response's output as the GA event sequence (realtime design §2.3,
//! §4.1, §7.4, §8.3).
//!
//! The session core feeds this the chat stream's deltas as they arrive and
//! it says, in order, what the client must hear:
//! - a **message** item: `output_item.added` (`in_progress`),
//!   `conversation.item.added`, `content_part.added`,
//!   `output_text.delta`*, then `output_text.done`, `content_part.done`,
//!   `output_item.done` and `conversation.item.done`;
//! - a **function call** item: `output_item.added` (`in_progress`, empty
//!   arguments), `conversation.item.added`, `function_call_arguments.delta`*,
//!   then `.done`, `output_item.done` (`completed` — the only event that
//!   says so for this call, since `@openai/agents` runs the tool on every
//!   one that does, §2.3) and `conversation.item.done`;
//! - then `response.done`.
//!
//! Text the model writes before a call goes out as its own message item,
//! closed before the call's item opens. Every item is appended to the
//! conversation the moment it is announced and updated in place, so the
//! conversation always holds what the client was told.
//!
//! A **speaking** response (`audio`) says its message as clauses of audio
//! instead of text deltas, and keeps the message open until the audio has
//! played; its function calls close at the end of generation as in text
//! mode.
//!
//! **A cancel takes no text back** (§4.3): what the item holds is what the
//! client was sent, so no text is queued purgeable — only audio is, and a
//! cancelled audio item keeps what left. A cancelled or failed response
//! closes its open items `incomplete`
//! with `output_item.done` and `conversation.item.done`
//! ([`Output::abandon`]) — `@openai/agents` updates its history only from
//! item events, and a call's `function_call_arguments.done` with half its
//! arguments would read as a call to run. A spoken message's audio part is
//! closed before them (`output_audio.done` and the rest, with what was
//! heard): the SDK resets its audio counters only there.
//!
//! `call_id`s are the upstream's own when it gives one this session has not
//! seen, and minted session-unique otherwise (§7.4) — some local models
//! number their calls per request, and the stock client refuses a reused
//! id. The minted id is what the conversation, the client and every later
//! rendering use; the upstream's original is logged beside it.

use std::collections::HashMap;

use serde_json::Value;

use super::conversation::Conversation;
use super::ids::Ids;
use super::protocol::{
    ContentPart, FunctionCallItem, Item, ItemStatus, MaxOutputTokens, MessageItem, Modality,
    PartRef, ResponseObject, ResponsePart, ResponseStatus, Role, ServerEvent, StatusDetails, Usage,
    ITEM_OBJECT,
};
use super::writer::Outbox;

mod audio;
mod closing;
mod usage;

pub(crate) use audio::{AudioOut, DELTA_MS};
pub use usage::{known_usage, usage};

/// `"realtime.response"`, the `object` of every response.
pub const RESPONSE_OBJECT: &str = "realtime.response";

/// One response's output, as far as it has gone.
pub(crate) struct Output {
    pub gen: u64,
    pub id: String,
    modalities: Vec<Modality>,
    max_output_tokens: MaxOutputTokens,
    metadata: Option<Value>,
    /// Item ids, by output index.
    items: Vec<String>,
    /// Whether each output index's closing events went out.
    closed: Vec<bool>,
    /// The message item text deltas — or clauses — go to.
    text_at: Option<u32>,
    /// The upstream's tool-call ordinal → output index.
    calls: HashMap<usize, u32>,
    /// How the response speaks; `None` for text output (§8.3).
    audio: Option<AudioOut>,
    /// The writer has the lead of this response's audio (`audio`).
    paced: bool,
    /// A text delta went out: text output is never purgeable, so the
    /// client has it (§4.3 "heard", B3 review 1).
    text_sent: bool,
}

impl Output {
    pub fn new(
        gen: u64,
        id: String,
        modalities: Vec<Modality>,
        max_output_tokens: MaxOutputTokens,
        metadata: Option<Value>,
        audio: Option<AudioOut>,
    ) -> Self {
        Self {
            gen,
            id,
            modalities,
            max_output_tokens,
            metadata,
            items: Vec::new(),
            closed: Vec::new(),
            text_at: None,
            calls: HashMap::new(),
            audio,
            paced: false,
            text_sent: false,
        }
    }

    /// `response.created`.
    pub fn created(&self, conv: &Conversation, ob: &mut Outbox) {
        ob.send(ServerEvent::ResponseCreated {
            response: Box::new(self.object(conv, ResponseStatus::InProgress, None, None)),
        });
    }

    /// One text delta: opens the message item on the first.
    pub fn text(&mut self, conv: &mut Conversation, ids: &Ids, ob: &mut Outbox, delta: &str) {
        let at = match self.text_at {
            Some(at) => at,
            None => self.open_message(conv, ids, ob),
        };
        let item_id = self.items[at as usize].clone();
        if let Some(Item::Message(m)) = conv.get_mut(&item_id) {
            if let Some(ContentPart::OutputText { text }) = m.content.first_mut() {
                text.push_str(delta);
            }
        }
        ob.send(ServerEvent::OutputTextDelta {
            at: self.part_ref(at),
            delta: delta.to_string(),
        });
        self.text_sent = true;
    }

    /// The response's message item — its text, or its spoken answer — once
    /// one opened.
    pub fn message_item(&self, conv: &Conversation) -> Option<String> {
        self.items
            .iter()
            .find(|id| matches!(conv.get(id), Some(Item::Message(_))))
            .cloned()
    }

    /// Whether a text delta of this response went out — what a text
    /// response's listener has heard. A spoken response's text goes out
    /// paced with its audio, and the writer says whether any of that left
    /// (`WriterHandle::heard`).
    pub fn text_sent(&self) -> bool {
        self.text_sent
    }

    fn open_message(&mut self, conv: &mut Conversation, ids: &Ids, ob: &mut Outbox) -> u32 {
        let item = Item::Message(MessageItem {
            id: Some(conv.fresh_item_id(ids)),
            object: Some(ITEM_OBJECT.into()),
            status: Some(ItemStatus::InProgress),
            role: Role::Assistant,
            content: Vec::new(),
        });
        let at = self.announce(conv, ob, item);
        let item_id = self.items[at as usize].clone();
        let (content, part) = if self.audio.is_some() {
            (
                ContentPart::OutputAudio {
                    audio: None,
                    transcript: Some(String::new()),
                },
                ResponsePart::Audio {
                    audio: None,
                    transcript: String::new(),
                },
            )
        } else {
            (
                ContentPart::OutputText {
                    text: String::new(),
                },
                ResponsePart::Text {
                    text: String::new(),
                },
            )
        };
        if let Some(Item::Message(m)) = conv.get_mut(&item_id) {
            m.content.push(content);
        }
        ob.send(ServerEvent::ContentPartAdded {
            at: self.part_ref(at),
            part,
        });
        self.text_at = Some(at);
        at
    }

    /// A tool call began: whatever text came first is closed as its own
    /// item, and the call's item opens with empty arguments.
    pub fn call_start(
        &mut self,
        conv: &mut Conversation,
        ids: &Ids,
        ob: &mut Outbox,
        index: usize,
        upstream_id: &str,
        name: &str,
    ) {
        // The text before a call is its own item, closed first — except a
        // spoken one, which closes when its audio has played.
        if let Some(at) = self.text_at.take().filter(|_| !self.speaks()) {
            self.close(conv, ob, at, ItemStatus::Completed);
        }
        let call_id = if !upstream_id.is_empty() && !conv.call_id_seen(upstream_id) {
            upstream_id.to_string()
        } else {
            let minted = conv.fresh_call_id(ids);
            tracing::debug!(
                "realtime response {}: call '{name}' given id {minted} (the upstream's was \
                 '{upstream_id}', already used in this session or empty)",
                self.id
            );
            minted
        };
        conv.note_call_id(&call_id);
        let item = Item::FunctionCall(FunctionCallItem {
            id: Some(conv.fresh_item_id(ids)),
            object: Some(ITEM_OBJECT.into()),
            status: Some(ItemStatus::InProgress),
            call_id: Some(call_id),
            name: name.to_string(),
            arguments: String::new(),
        });
        let at = self.announce(conv, ob, item);
        self.calls.insert(index, at);
    }

    /// A fragment of a call's arguments.
    pub fn call_args(
        &mut self,
        conv: &mut Conversation,
        ob: &mut Outbox,
        index: usize,
        delta: &str,
    ) {
        let Some(&at) = self.calls.get(&index) else {
            // The stream never named this call (no `name` arrived), so there
            // is no item to carry its arguments.
            tracing::debug!(
                "realtime response {}: arguments for unannounced call #{index} dropped",
                self.id
            );
            return;
        };
        let item_id = self.items[at as usize].clone();
        let Some(Item::FunctionCall(c)) = conv.get_mut(&item_id) else {
            return;
        };
        c.arguments.push_str(delta);
        let call_id = c.call_id.clone().unwrap_or_default();
        ob.send(ServerEvent::FunctionCallArgumentsDelta {
            response_id: self.id.clone(),
            item_id,
            output_index: at,
            call_id,
            delta: delta.to_string(),
        });
    }

    /// `response.done`.
    pub fn done(
        &self,
        conv: &Conversation,
        ob: &mut Outbox,
        status: ResponseStatus,
        details: Option<StatusDetails>,
        usage: Option<Usage>,
    ) {
        ob.send(ServerEvent::ResponseDone {
            response: Box::new(self.object(conv, status, details, usage)),
        });
    }

    /// Append `item` to the conversation and announce it.
    fn announce(&mut self, conv: &mut Conversation, ob: &mut Outbox, item: Item) -> u32 {
        let at = self.items.len() as u32;
        self.items.push(item.id().unwrap_or_default().to_string());
        self.closed.push(false);
        let previous = conv.append(item.clone());
        ob.send(ServerEvent::OutputItemAdded {
            response_id: self.id.clone(),
            output_index: at,
            item: item.clone(),
        });
        ob.send(ServerEvent::ItemAdded {
            previous_item_id: previous,
            item,
        });
        at
    }

    pub(super) fn part_ref(&self, at: u32) -> PartRef {
        PartRef {
            response_id: self.id.clone(),
            item_id: self.items[at as usize].clone(),
            output_index: at,
            content_index: 0,
        }
    }

    fn object(
        &self,
        conv: &Conversation,
        status: ResponseStatus,
        status_details: Option<StatusDetails>,
        usage: Option<Usage>,
    ) -> ResponseObject {
        ResponseObject {
            id: self.id.clone(),
            object: RESPONSE_OBJECT.into(),
            status,
            status_details,
            // An item the client deleted after it was done is no longer
            // part of what this response left in the conversation.
            output: self
                .items
                .iter()
                .filter_map(|id| conv.get(id).cloned())
                .collect(),
            conversation_id: None,
            output_modalities: self.modalities.clone(),
            max_output_tokens: self.max_output_tokens,
            audio: self.audio_object(),
            usage,
            metadata: self.metadata.clone(),
        }
    }
}
