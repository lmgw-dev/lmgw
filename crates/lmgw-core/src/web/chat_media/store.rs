//! A Chat thread's side of the media tools' refs (chat-media-tools design
//! §3.6, §6.3).
//!
//! A card action's picture (`source = 'action'`) is not the model's (the
//! owner's decision, design §8.2): `att:N` never resolves to one and a
//! refusal never lists one. The user gives it to the model by **Attach**,
//! which puts a copy in the composer as an upload (§8.2) — the copy has an
//! id of its own, and that one resolves like any upload.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lmgw_api_types::chat_media::{SOURCE_ACTION, SOURCE_TOOL};
use lmgw_api_types::chat_threads::{AttachmentFacts, AttachmentMeta};

use crate::erased::Erased;
use crate::mcp::media::{Produced, Refs, Source};
use crate::state::SharedState;
use crate::store::NewAttachment;

use super::super::chat_repo::ChatRepo;

/// The thread's files for one turn's media calls.
pub(crate) struct ThreadFiles {
    state: SharedState,
    repo: ChatRepo,
    thread_id: i64,
    /// What the turn's calls stored, in order: bound to the reply when it
    /// is saved.
    stored: Mutex<Vec<i64>>,
    /// Each call's files as the API carries them, until its result frame
    /// takes them ([`Self::take_made`]).
    made: Mutex<HashMap<String, Vec<AttachmentMeta>>>,
}

impl ThreadFiles {
    pub fn new(state: SharedState, repo: ChatRepo, thread_id: i64) -> Arc<Self> {
        Arc::new(Self {
            state,
            repo,
            thread_id,
            stored: Mutex::new(Vec::new()),
            made: Mutex::new(HashMap::new()),
        })
    }

    /// The files call `call_id` stored, for its result frame
    /// (`ToolResultFrame::attachments`, design §6.2); each is handed out
    /// once.
    pub fn take_made(&self, call_id: &str) -> Vec<AttachmentMeta> {
        self.made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(call_id)
            .unwrap_or_default()
    }

    /// The ids the turn's calls stored.
    pub fn stored(&self) -> Vec<i64> {
        self.stored
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The thread's `kind` files a model may name, as refs, for a refusal.
    async fn named(&self, kind: &str) -> String {
        let mut ids: Vec<i64> = self
            .repo
            .attachments_meta(&self.state, self.thread_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|a| a.kind == kind && a.source != SOURCE_ACTION)
            .map(|a| a.id)
            .collect();
        // This turn's own, not bound to its reply yet.
        let own = self.stored();
        let own = self
            .repo
            .attachments_by_ids(&self.state, self.thread_id, &own)
            .await
            .unwrap_or_default();
        ids.extend(own.iter().filter(|a| a.kind == kind).map(|a| a.id));
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            format!("this conversation has no {kind} to name")
        } else {
            format!(
                "this conversation's {kind} refs are {}",
                ids.iter()
                    .map(|i| format!("att:{i}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

impl Refs for ThreadFiles {
    fn attachment<'a>(&'a self, id: i64, kind: &'a str) -> Erased<'a, Result<Source, String>> {
        Box::pin(async move {
            // Of this thread only: another thread's id is simply not found.
            let found = self
                .repo
                .attachments_by_ids(&self.state, self.thread_id, &[id])
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                // A card action's picture is not the model's (module doc).
                .find(|a| a.source != SOURCE_ACTION);
            match found {
                Some(a) if a.kind == kind => Ok(Source {
                    made_by: made_by(&a.meta),
                    mime: a.mime,
                    name: a.name,
                    bytes: a.data,
                    id: Some(a.id),
                }),
                Some(a) => Err(format!(
                    "att:{id} is {} {}, not {kind}; {}",
                    article(&a.kind),
                    a.kind,
                    self.named(kind).await
                )),
                None => Err(format!(
                    "there is no att:{id} in this conversation; {}",
                    self.named(kind).await
                )),
            }
        })
    }

    fn store<'a>(
        &'a self,
        call_id: &'a str,
        files: &'a [Produced],
    ) -> Erased<'a, Option<Result<Vec<i64>, String>>> {
        Box::pin(async move {
            let mut ids = Vec::with_capacity(files.len());
            for f in files {
                let new = NewAttachment {
                    kind: f.kind.to_string(),
                    name: f.name.clone(),
                    mime: f.mime.clone(),
                    data: f.bytes.clone(),
                    extracted: None,
                    meta: serde_json::to_value(AttachmentFacts {
                        media: Some(f.facts.clone()),
                        ..Default::default()
                    })
                    .unwrap_or_default(),
                    mode: None,
                    source: SOURCE_TOOL.to_string(),
                    call_id: (!call_id.is_empty()).then(|| call_id.to_string()),
                };
                match self
                    .repo
                    .insert_tool_file(&self.state, self.thread_id, &new)
                    .await
                {
                    Ok(id) => {
                        ids.push(id);
                        self.stored
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(id);
                        self.made
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .entry(call_id.to_string())
                            .or_default()
                            .push(AttachmentMeta {
                                id,
                                kind: new.kind.clone(),
                                name: new.name.clone(),
                                mime: new.mime.clone(),
                                size: new.data.len() as i64,
                                meta: new.meta.clone(),
                                source: new.source.clone(),
                                call_id: new.call_id.clone(),
                                ..Default::default()
                            });
                    }
                    Err(e) => return Some(Err(e.to_string())),
                }
            }
            Some(Ok(ids))
        })
    }
}

/// The model a media call made a file with, from its `meta.media`.
pub(super) fn made_by(meta: &serde_json::Value) -> Option<String> {
    serde_json::from_value::<AttachmentFacts>(meta.clone())
        .ok()
        .and_then(|f| f.media)
        .and_then(|m| m.model)
}

fn article(kind: &str) -> &'static str {
    if kind.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_card_actions_picture_never_resolves_and_is_never_listed() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let t = crate::store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        let png = crate::bench::png::solid_red();
        let upload = crate::store::insert_chat_attachment_new(
            &state.db,
            t,
            &NewAttachment::plain("image", "cat.png", "image/png", &png),
        )
        .await
        .unwrap();
        let mut made = NewAttachment::plain("image", "cat-x4.png", "image/png", &png);
        made.source = SOURCE_ACTION.into();
        made.call_id = Some("act-0-1".into());
        let action = crate::store::insert_chat_attachment_new(&state.db, t, &made)
            .await
            .unwrap();
        let m = crate::store::append_user_message_with_attachments(&state.db, t, "look", &[upload])
            .await
            .unwrap();
        let crate::store::SendMessageOutcome::Sent(mid) = m else {
            panic!("not sent");
        };
        assert!(crate::store::bind_action_file(&state.db, t, mid, action)
            .await
            .unwrap());

        let files = ThreadFiles::new(state.clone(), ChatRepo::Db, t);
        let found = files.attachment(upload, "image").await.unwrap();
        assert_eq!(found.id, Some(upload));
        let refused = files.attachment(action, "image").await.unwrap_err();
        assert_eq!(
            refused,
            format!(
                "there is no att:{action} in this conversation; this conversation's image \
                 refs are att:{upload}"
            )
        );
    }
}
