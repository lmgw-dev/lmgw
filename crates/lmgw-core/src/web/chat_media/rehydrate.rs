//! A turn record's stored pictures back as pictures (chat-media-tools
//! design §6.4).
//!
//! The record (`ir_messages`) keeps `Resource{uri: "lmgw:attachment/57"}`
//! where a tool's picture was, never its base64. Before a request goes out
//! each such resource becomes the picture it names — the history's when the
//! turn's request is built, a tool call's of this turn before each model
//! call — so the model that answers sees what its earlier calls made. What
//! happens after is the existing rule: the egress sends a tool-result image
//! where its protocol and the model take one, and a placeholder elsewhere.
//!
//! A ref whose attachment is gone becomes the text `[picture att:57 was
//! deleted]`; one naming anything but a picture goes as its ref alone.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine as _;

use crate::ir::{ContentPart, Message, ToolResultBlock};
use crate::mcp::media::result::attachment_of_uri;
use crate::state::SharedState;

use super::super::chat_repo::ChatRepo;

/// What a ref resolved to.
#[derive(Debug, Clone)]
enum Found {
    Picture { mime: String, data: String },
    Other,
    Gone,
}

/// One thread's refs, resolved once each per turn.
pub(crate) struct Hydrator {
    state: SharedState,
    repo: ChatRepo,
    thread_id: i64,
    seen: Mutex<HashMap<i64, Found>>,
}

/// The stored-file refs among `messages`' tool results.
fn refs_in(messages: &[Message]) -> Vec<i64> {
    let mut out = Vec::new();
    for m in messages {
        for p in &m.content {
            if let ContentPart::ToolResult { content, .. } = p {
                for b in content {
                    if let ToolResultBlock::Resource { uri, .. } = b {
                        if let Some(id) = attachment_of_uri(uri) {
                            out.push(id);
                        }
                    }
                }
            }
        }
    }
    out
}

impl Hydrator {
    pub fn new(state: SharedState, repo: ChatRepo, thread_id: i64) -> Self {
        Self {
            state,
            repo,
            thread_id,
            seen: Mutex::new(HashMap::new()),
        }
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Found>> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Resolve every ref of `ids` not resolved yet, in one read.
    async fn resolve(&self, ids: &[i64]) {
        let missing: Vec<i64> = {
            let seen = self.seen();
            ids.iter()
                .copied()
                .filter(|i| !seen.contains_key(i))
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let rows = self
            .repo
            .attachments_by_ids(&self.state, self.thread_id, &missing)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    thread = self.thread_id,
                    "chat: the pictures of a turn record could not be read, they go as notes: {e}"
                );
                Vec::new()
            });
        let mut seen = self.seen();
        for id in missing {
            let found = match rows.iter().find(|a| a.id == id) {
                Some(a) if a.kind == "image" => Found::Picture {
                    mime: a.mime.clone(),
                    data: base64::engine::general_purpose::STANDARD.encode(&a.data),
                },
                Some(_) => Found::Other,
                None => Found::Gone,
            };
            seen.insert(id, found);
        }
    }

    /// Turn every stored-file ref in `messages` into what it names. `true`
    /// when anything changed.
    pub async fn hydrate(&self, messages: &mut [Message]) -> bool {
        let ids = refs_in(messages);
        if ids.is_empty() {
            return false;
        }
        self.resolve(&ids).await;
        let seen = self.seen();
        for m in messages.iter_mut() {
            for p in m.content.iter_mut() {
                let ContentPart::ToolResult { content, .. } = p else {
                    continue;
                };
                for b in content.iter_mut() {
                    let ToolResultBlock::Resource { uri, .. } = b else {
                        continue;
                    };
                    let Some(id) = attachment_of_uri(uri) else {
                        continue;
                    };
                    *b = match seen.get(&id) {
                        Some(Found::Picture { mime, data }) => ToolResultBlock::Image {
                            mime: mime.clone(),
                            data: data.clone(),
                        },
                        Some(Found::Other) => ToolResultBlock::text(format!("[att:{id}]")),
                        Some(Found::Gone) | None => {
                            ToolResultBlock::text(format!("[picture att:{id} was deleted]"))
                        }
                    };
                }
            }
        }
        true
    }

    /// `messages` with their refs hydrated, when they hold any.
    pub async fn hydrated(&self, messages: &[Message]) -> Option<Vec<Message>> {
        if refs_in(messages).is_empty() {
            return None;
        }
        let mut out = messages.to_vec();
        self.hydrate(&mut out).await;
        Some(out)
    }
}
