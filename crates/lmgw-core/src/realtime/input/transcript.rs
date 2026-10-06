//! A committed turn's transcript, as the session core takes it (realtime
//! design §4.2 step 3, §4.3, §5.2): the transcription events, the user
//! item's `done`, and what was waiting for it — the owed automatic response
//! and a response awaiting transcripts. A turn transcribed a second time,
//! with the word check's alias after its own came back empty (§6.4, N3),
//! says so at INFO.
//!
//! **A turn the chat model heard** (voice-audio-input design §3.1, §3.2)
//! leaves the hearing first: from here its transcript says whether it has
//! words. A failed transcription of one a model heard — its response is
//! active, and its attempt carried the audio — is logged at INFO and kept
//! for its user row, but not sent as `transcription.failed` or `error`: the
//! reply plays, and the page would say none comes. Any other failure is
//! said as with audio input off: a turn handed to a response that was cut,
//! skipped or refused, or whose model has not answered yet, and a failure
//! that is no transcription at all ([`attempted`]: no
//! speech-to-text model, the key's policy, the session's stop; WP3 review
//! #2, #3). Once a response's last audio turn is in, its hold is settled
//! (`lifecycle::hearing`).

use super::super::conversation::Conversation;
use super::super::protocol::{
    ContentPart, ErrorObject, Item, ServerEvent, Session, TranscriptionUsage,
};
use super::super::session::Core;
use super::super::thread::hearing::{Hearing, Was};
use super::super::transcribe::{Again, Done};
use crate::error::GatewayError;

/// Whether a response that waited for the `awaited` turns would answer
/// nothing that was said, because transcription failed (§4.3, WP2 review
/// R10): at least one of them has no transcript at all — its ASR call
/// failed — none of them has words, and **nothing newer than the first
/// failed one** is something to answer. Turns the client deleted meanwhile
/// do not count.
///
/// "Newer" is the conversation's order, not the awaited list's: a client's
/// `response.create` answering a `function_call_output` waits for a turn
/// that was still being transcribed when the output arrived, and must not
/// fail because that unrelated turn did (WP1c review #1); a turn committed
/// after the failed one, with words, is what the response answers.
///
/// A turn the chat model hears (`hearing`, voice-audio-input design §3.1)
/// has words until its transcript says otherwise.
pub(in crate::realtime) fn transcripts_failed(
    conv: &Conversation,
    awaited: &[String],
    hearing: Option<&Hearing>,
) -> bool {
    let items = conv.items();
    let mut first_failed: Option<usize> = None;
    for id in awaited {
        let Some(at) = items.iter().position(|i| i.id() == Some(id.as_str())) else {
            continue;
        };
        if has_words(&items[at], hearing) {
            return false;
        }
        if untranscribed(&items[at]) {
            first_failed = Some(first_failed.map_or(at, |f| f.min(at)));
        }
    }
    let Some(from) = first_failed else {
        return false;
    };
    !items[from + 1..].iter().any(|i| answerable(i, hearing))
}

/// Whether a message says anything: typed text, or a transcript with words
/// — or it is a turn the chat model hears whose transcript is not in yet
/// (`hearing`, voice-audio-input design §3.1): it counts as having words
/// until it is transcribed.
pub(in crate::realtime) fn has_words(item: &Item, hearing: Option<&Hearing>) -> bool {
    let Item::Message(m) = item else {
        return false;
    };
    if let (Some(h), Some(id)) = (hearing, m.id.as_deref()) {
        if h.hears(id) {
            return true;
        }
    }
    m.content.iter().any(|p| match p {
        ContentPart::InputText { text } => !text.trim().is_empty(),
        ContentPart::InputAudio { transcript, .. } => {
            transcript.as_deref().is_some_and(|t| !t.trim().is_empty())
        }
        _ => false,
    })
}

/// Something a response answers: words, or a tool's output — a client
/// function's, or a server-side call's result (realtime-server-tools §2.6).
fn answerable(item: &Item, hearing: Option<&Hearing>) -> bool {
    has_words(item, hearing)
        || match item {
            Item::FunctionCallOutput(_) => true,
            Item::McpCall(c) => c.output.is_some() || c.error.is_some(),
            _ => false,
        }
}

/// A committed turn whose ASR call failed: audio with no transcript.
fn untranscribed(item: &Item) -> bool {
    let Item::Message(m) = item else {
        return false;
    };
    m.content.iter().any(|p| {
        matches!(
            p,
            ContentPart::InputAudio {
                transcript: None,
                ..
            }
        )
    })
}

/// Whether a turn call's failure `e` is the transcription's own — the
/// speech-to-text model was asked and failed — rather than a call never
/// made (voice-audio-input design §3.2, WP3 review #2): the thread names no
/// transcription model (`asr_not_configured`), the session key's policy
/// refused the call, or the session's stop ended it. A turn the chat model
/// heard is heard only when its transcription was attempted: otherwise its
/// failure is said as with audio input off, and nobody writes it as "not
/// transcribed".
pub(in crate::realtime) fn attempted(e: &GatewayError) -> bool {
    !matches!(
        e,
        GatewayError::InvalidRequest {
            code: "asr_not_configured",
            ..
        } | GatewayError::Unauthorized(_)
            | GatewayError::AgentDisabled { .. }
            | GatewayError::KeyScope { .. }
            | GatewayError::KeyBudget { .. }
            | GatewayError::KeyRate { .. }
            | GatewayError::KeyExpired { .. }
    ) && !crate::proxy::is_canceled(e)
}

/// Whether the session asked for transcription events (§5.2).
fn transcription_events(s: &Session) -> bool {
    s.audio
        .as_ref()
        .and_then(|a| a.input.as_ref())
        .is_some_and(|i| i.transcription.is_some())
}

impl Core {
    /// A turn's ASR call is over: the transcript goes into its item, the
    /// events out, and what waited for it goes on.
    pub(in crate::realtime) fn on_transcript(&mut self, done: Done) {
        if !self.transcriber.finished(&done.item_id) {
            return;
        }
        if let Some(again) = &done.again {
            self.log_again(&done.item_id, again, &done.result);
        }
        let was = self.hearing_transcribed(&done.item_id);
        self.bound_transcribed(&done, matches!(was, Was::Launched { .. }));
        // A model hears it now (module doc).
        let heard = match &was {
            Was::Launched { gen, last } => self.heard_now(*gen, last.as_ref()),
            _ => false,
        };
        let events = transcription_events(&self.session);
        let transcript = match done.result {
            Ok(text) => {
                if events {
                    self.ob.send(ServerEvent::TranscriptionCompleted {
                        item_id: done.item_id.clone(),
                        content_index: 0,
                        transcript: text.clone(),
                        usage: TranscriptionUsage::Duration {
                            seconds: done.seconds,
                        },
                    });
                }
                Some(text)
            }
            Err(e) if heard && attempted(&e) => {
                tracing::info!(
                    "realtime {}: transcribing {} failed — {e}; the chat model heard it, so its \
                     reply plays, and its user message says it was not transcribed",
                    self.id(),
                    done.item_id
                );
                None
            }
            Err(e) => {
                tracing::info!(
                    "realtime {}: transcribing {} failed — {e}",
                    self.id(),
                    done.item_id
                );
                let error = ErrorObject::from_gateway(&e);
                if events {
                    self.ob.send(ServerEvent::TranscriptionFailed {
                        item_id: done.item_id.clone(),
                        content_index: 0,
                        error: error.into(),
                    });
                } else {
                    // Not asked for transcription events, but never silent.
                    self.error(error);
                }
                None
            }
        };
        if let Some(Item::Message(m)) = self.conversation.get_mut(&done.item_id) {
            if let Some(ContentPart::InputAudio { transcript: t, .. }) = m.content.first_mut() {
                t.clone_from(&transcript);
            }
            let item = Item::Message(m.clone());
            self.ob.send(ServerEvent::ItemDone {
                previous_item_id: self.conversation.previous_of(&done.item_id),
                item,
            });
        }
        self.timing_transcribed(&done.item_id);
        let hearing = self.hearing();
        if !self
            .conversation
            .get(&done.item_id)
            .is_some_and(|i| has_words(i, hearing))
        {
            self.timing_no_words(&done.item_id);
        }
        self.pending_resolve(&done.item_id, was != Was::NotHeard);
        if let Was::Launched {
            gen,
            last: Some(launched),
        } = was
        {
            self.heard_settled(gen, launched, true);
        }
        if !self.busy_for_launch() {
            self.launch();
        }
    }
}

impl Core {
    /// A committed turn's transcript that came in after a bound session
    /// ended (`lifecycle::bound`'s `end_bound_turns`): kept on its item for
    /// the journal, and nothing else — no event (nobody reads them now), and
    /// no response starts.
    pub(in crate::realtime) fn on_last_transcript(&mut self, done: Done) {
        if !self.transcriber.finished(&done.item_id) {
            return;
        }
        let was = self.hearing_transcribed(&done.item_id);
        self.bound_transcribed(&done, matches!(was, Was::Launched { .. }));
        let carried = match &was {
            Was::Launched { gen, last } => match last {
                Some(l) => l.carried,
                None => self.hearing().and_then(|h| h.carried(*gen)),
            },
            _ => None,
        };
        let transcript = match done.result {
            Ok(text) => Some(text),
            Err(e) => {
                tracing::info!(
                    "realtime {}: transcribing {} failed as the session ended — {e}{}",
                    self.id(),
                    done.item_id,
                    if carried == Some(true) && attempted(&e) {
                        "; the chat model heard it, and its user message says it was not \
                         transcribed"
                    } else {
                        "; it is not written"
                    }
                );
                None
            }
        };
        if let Some(Item::Message(m)) = self.conversation.get_mut(&done.item_id) {
            if let Some(ContentPart::InputAudio { transcript: t, .. }) = m.content.first_mut() {
                *t = transcript;
            }
        }
        // A response that heard it is over: its journal entry still gets its
        // row (voice-audio-input design §3.3).
        if let Was::Launched {
            gen,
            last: Some(launched),
        } = was
        {
            self.heard_settled(gen, launched, false);
        }
    }

    /// The second transcription of `item_id` (module doc): what it heard,
    /// or why the turn stays empty.
    fn log_again(&self, item_id: &str, again: &Again, result: &Result<String, GatewayError>) {
        let Again { asr, check, failed } = again;
        let why = format!(
            "{item_id} came back empty from {asr}, while the barge-in word check's {check} had \
             heard words in it"
        );
        match failed {
            Some(e) => tracing::info!(
                "realtime {}: {why} — transcribing it again with {check} failed ({e}); the turn \
                 stays empty",
                self.id()
            ),
            None => tracing::info!(
                "realtime {}: {why} — transcribed again with {check} (one more ASR call, a usage \
                 row of its own): {:?}",
                self.id(),
                result.as_deref().unwrap_or_default()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::super::conversation::Conversation;
    use super::super::super::protocol::Item;
    use super::transcripts_failed;

    fn item(v: serde_json::Value) -> Item {
        serde_json::from_value(v).unwrap()
    }

    /// A server-side call's result after a turn that failed to transcribe
    /// is something to answer, as a function's output is; a call still
    /// without one is not (realtime-server-tools §2.6).
    #[test]
    fn an_mcp_call_with_a_result_is_answerable() {
        let failed = item(json!({"type": "message", "id": "item_u", "role": "user",
                                 "content": [{"type": "input_audio"}]}));
        let call = |result: serde_json::Value| {
            let mut v = json!({"type": "mcp_call", "id": "item_c", "server_label": "a",
                               "name": "echo", "arguments": "{}"});
            if let serde_json::Value::Object(r) = result {
                v.as_object_mut().unwrap().extend(r);
            }
            item(v)
        };
        let awaited = ["item_u".to_string()];
        for (result, answerable) in [
            (json!({"output": "done"}), true),
            (
                json!({"error": {"type": "tool_execution_error", "message": "no"}}),
                true,
            ),
            (json!({}), false),
        ] {
            let mut conv = Conversation::default();
            conv.append(failed.clone());
            conv.append(call(result.clone()));
            assert_eq!(
                transcripts_failed(&conv, &awaited, None),
                !answerable,
                "{result}"
            );
        }
    }
}
