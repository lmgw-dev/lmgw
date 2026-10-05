//! The session core's client events (realtime design §2.2, §2.3, §4.1):
//! one text frame parsed and dispatched, and `session.update`.

use serde_json::{Map, Value};

use super::super::asr::AsrResolution;
use super::super::expressive;
use super::super::input::{barge_params, detector_params, log_turn_detection};
use super::super::protocol::{ClientEvent, ErrorObject, ServerEvent, Session};
use super::super::resolve::{self, ChatResolution};
use super::super::{merge, policy};
use super::speech::Speech;
use super::Core;
use crate::telemetry::RequestClass;

impl Core {
    /// One text frame. Unparseable JSON, an unknown `type` or a known event
    /// of the wrong shape all answer `invalid_event`, echoing the frame's
    /// `event_id` when it had one, and the session goes on (§4.1).
    /// `arrived`: when the frame came off the socket (`input_append`).
    pub(super) async fn on_text(&mut self, text: &str, arrived: tokio::time::Instant) {
        let value: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                return self.error(ErrorObject::invalid(
                    "invalid_event",
                    format!("the frame is not JSON: {e}"),
                ))
            }
        };
        let event_id = value
            .get("event_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        match serde_json::from_value::<ClientEvent>(value) {
            Ok(ev) => self.on_event(ev, arrived).await,
            Err(e) => {
                let err = match kind {
                    None => ErrorObject::invalid("invalid_event", "the event has no `type`")
                        .with_param("type"),
                    Some(k) if !is_client_type(&k) => ErrorObject::invalid(
                        "invalid_event",
                        format!("'{k}' is not a client event of the GA Realtime protocol"),
                    )
                    .with_param("type"),
                    Some(k) => ErrorObject::invalid("invalid_event", format!("{k}: {e}")),
                };
                self.error(err.for_event(event_id.as_deref()));
            }
        }
    }

    async fn on_event(&mut self, ev: ClientEvent, arrived: tokio::time::Instant) {
        match ev {
            ClientEvent::SessionUpdate { event_id, session } => {
                self.session_update(event_id.as_deref(), &session).await
            }
            // The thread's history is a bound session's conversation (§8.1).
            ClientEvent::ConversationItemCreate { event_id, .. } if self.bound.is_some() => self
                .error(
                    super::super::thread::refuse_item("conversation.item.create")
                        .for_event(event_id.as_deref()),
                ),
            ClientEvent::ConversationItemDelete { event_id, .. } if self.bound.is_some() => self
                .error(
                    super::super::thread::refuse_item("conversation.item.delete")
                        .for_event(event_id.as_deref()),
                ),
            ClientEvent::ConversationItemCreate {
                event_id,
                previous_item_id,
                item,
            } => {
                match self
                    .conversation
                    .create(item, previous_item_id.as_deref(), &self.ids)
                {
                    Ok(events) => events.into_iter().for_each(|e| self.ob.send(e)),
                    Err(e) => self.error(e.for_event(event_id.as_deref())),
                }
            }
            ClientEvent::ConversationItemRetrieve { event_id, item_id } => {
                let answer = self.conversation.retrieve(&item_id);
                self.answer(answer, event_id.as_deref())
            }
            ClientEvent::ConversationItemDelete { event_id, item_id } => {
                let answer = self.conversation.delete(&item_id);
                self.answer(answer, event_id.as_deref())
            }
            ClientEvent::ConversationItemTruncate {
                event_id,
                item_id,
                content_index,
                audio_end_ms,
            } => self.item_truncate(event_id.as_deref(), &item_id, content_index, audio_end_ms),
            ClientEvent::ResponseCreate { event_id, response } => {
                self.response_create(event_id, response)
            }
            ClientEvent::ResponseCancel {
                event_id,
                response_id,
            } => self.response_cancel(event_id.as_deref(), response_id.as_deref()),
            ClientEvent::InputAudioBufferAppend { event_id, audio } => {
                self.input_append(event_id.as_deref(), &audio, arrived)
                    .await
            }
            ClientEvent::InputAudioBufferCommit { event_id } => {
                self.input_commit(event_id.as_deref())
            }
            ClientEvent::InputAudioBufferClear { .. } => self.input_clear(),
            // WebRTC only in OpenAI's API (§2.3). Answered rather than
            // ignored, so a client never waits on an event this build drops.
            other @ ClientEvent::OutputAudioBufferClear { .. } => self.error(
                ErrorObject::invalid(
                    "not_implemented_yet",
                    format!(
                        "{} is not implemented yet in this lmgw build (realtime work in \
                         progress: audio arrives after text conversations)",
                        other.type_name()
                    ),
                )
                .for_event(other.event_id()),
            ),
        }
    }

    /// One event's answer, or its error echoing the client's `event_id`.
    fn answer(&mut self, answer: Result<ServerEvent, ErrorObject>, event_id: Option<&str>) {
        match answer {
            Ok(ev) => self.ob.send(ev),
            Err(e) => self.error(e.for_event(event_id)),
        }
    }

    /// `session.update` (§2.2): merge, re-resolve the chat, ASR and TTS
    /// models and the voice if their names changed and re-run the key's
    /// check for an alias that did (§10.3), retune the turn detector, then
    /// answer with the full session — or one `error`, leaving the session as
    /// it was.
    async fn session_update(&mut self, event_id: Option<&str>, update: &Map<String, Value>) {
        let snap = self.state.snapshot();
        let mut next = match merge::apply_update(&self.session, update, &snap.settings.realtime) {
            Ok(s) => s,
            Err(e) => return self.error(e.for_event(event_id)),
        };
        // What a bound session's thread owns stays the thread's (§8.1).
        if self.bound.is_some() {
            if let Err(e) = super::super::thread::check_update(&self.session, &next, update) {
                return self.error(e.for_event(event_id));
            }
        }
        let asr = match self.update_asr(&next).await {
            Ok(a) => a,
            Err(e) => return self.error(e.for_event(event_id)),
        };
        let speech = match self.update_speech(&next).await {
            Ok(s) => s,
            Err(e) => return self.error(e.for_event(event_id)),
        };
        // A word-check alias of the client's own: the key's check now, as
        // for a new model, rather than a refusal at every barge-in — and
        // then whether it is an ASR alias at all (B6), which an alias out of
        // the key's scope is not told.
        let own_check = |s: &Session| {
            s.lmgw
                .as_ref()
                .and_then(|l| l.barge_in_check_alias.clone())
                .filter(|a| !a.trim().is_empty())
        };
        if let Some(alias) =
            own_check(&next).filter(|a| Some(a) != own_check(&self.session).as_ref())
        {
            if let Err(e) =
                policy::check(&self.state, &self.ctx, alias.trim(), RequestClass::Audio).await
            {
                return self.error(
                    ErrorObject::from_gateway(&e)
                        .with_param("session.lmgw.barge_in_check_alias")
                        .for_event(event_id),
                );
            }
            if let Err(e) =
                super::super::input::vet_client_check_alias(&self.state, alias.trim()).await
            {
                return self.error(e.for_event(event_id));
            }
        }

        let mut chat = self.chat.clone();
        if next.model != self.session.model {
            chat = match resolve::resolve_chat(&snap, next.model.as_deref()) {
                Ok(r) => r,
                Err(e) => {
                    return self.error(
                        ErrorObject::from_gateway(&e)
                            .with_param("session.model")
                            .for_event(event_id),
                    )
                }
            };
            if let Some(alias) = chat
                .alias
                .as_deref()
                .filter(|_| chat.alias != self.chat.alias)
            {
                if let Err(e) =
                    policy::check(&self.state, &self.ctx, alias, RequestClass::Chat).await
                {
                    return self.error(
                        ErrorObject::from_gateway(&e)
                            .with_param("session.model")
                            .for_event(event_id),
                    );
                }
            }
            resolve::log_resolution(self.id(), next.model.as_deref(), &chat);
        }
        // Last, because it changes the detector: everything that can refuse
        // this update has had its say.
        let params = detector_params(&next, &snap.settings.realtime);
        if let Err(e) = self.input.configure(params.as_ref()) {
            return self.error(
                ErrorObject::invalid("invalid_value", e.to_string())
                    .with_param("session.audio.input.turn_detection")
                    .for_event(event_id),
            );
        }

        let barge = barge_params(&next, &snap.settings.realtime);
        let check = super::super::input::check_alias(&next, asr.alias.as_deref());
        self.input.set_barge_in(&barge, check.is_some());

        set_resolved(
            &mut next,
            &chat,
            &asr,
            &speech,
            (&snap.settings.realtime, self.seed),
        );
        log_turn_detection(self.id(), Some(&self.session), &next, params.as_ref());
        self.chat = chat;
        self.asr = asr;
        self.speech = speech;
        self.session = next;
        self.ob.send(ServerEvent::SessionUpdated {
            session: Box::new(self.session.clone()),
        });
        self.warn_check_scripts();
        // Turn detection off: an open turn ends here — its audio is the
        // manual buffer now, for the client to commit (§6.6) — and what it
        // deferred will not wait for a commit that never comes by itself
        // (§4.3): an owed response decides, a held create starts.
        if !self.input.detecting() {
            if let Some(turn) = self.turn.take() {
                self.ob.send(ServerEvent::SpeechStopped {
                    audio_end_ms: self.input.end_ms(),
                    item_id: turn.item_id,
                });
            }
            self.pending_undefer();
        }
    }
}

/// Echo the resolutions in `session.lmgw.resolved` (§5.1–§5.3), and what
/// the TTS alias speaks with (WP10 D3) — under `realtime`'s settings and the
/// seed lmgw drew for the session (`expressive`).
pub(super) fn set_resolved(
    s: &mut Session,
    chat: &ChatResolution,
    asr: &AsrResolution,
    speech: &Speech,
    (settings, seed): (&crate::config::RealtimeSettings, u32),
) {
    let expressive = speech.tts.alias.as_deref().map(|alias| {
        let r = expressive::resolved(&speech.expressive, s, settings, seed);
        let sid = s.id.as_deref().unwrap_or("?");
        expressive::log_resolution(sid, alias, r.dropped, r.source.as_deref());
        r
    });
    let lmgw = s.lmgw.get_or_insert_with(Default::default);
    let resolved = lmgw.resolved.get_or_insert_with(Default::default);
    resolved.chat = chat.alias.clone();
    resolved.asr = asr.alias.clone();
    resolved.tts = speech.tts.alias.clone();
    resolved.voice = speech.voice.name();
    resolved.speech = expressive;
}

/// The eleven client event types (§2.3), for telling an unknown `type` from
/// a known event of the wrong shape.
fn is_client_type(t: &str) -> bool {
    matches!(
        t,
        "session.update"
            | "input_audio_buffer.append"
            | "input_audio_buffer.commit"
            | "input_audio_buffer.clear"
            | "conversation.item.create"
            | "conversation.item.retrieve"
            | "conversation.item.truncate"
            | "conversation.item.delete"
            | "response.create"
            | "response.cancel"
            | "output_audio_buffer.clear"
    )
}
