//! Assistant audio items and what the listener heard of them (realtime
//! design §7.3).
//!
//! Every assistant audio item this session speaks has a [`HeardTable`]: each
//! clause's text and its samples, recorded as the clause is queued. The
//! item's transcript is always the table's text, so a `retrieve` mid-answer
//! finds what has been queued, and the renderer — which reads the
//! transcript — sends the model what was heard once the table is cut:
//! - by `conversation.item.truncate {audio_end_ms}`: what the client played;
//! - by a cancel without a truncate: what the writer sent (`keep`), which
//!   with pacing is what was played plus at most the lead (§8.2).
//!
//! The table also keeps what the model wrote for each clause, and the
//! renderer gives the model that — up to the same cut — rather than the
//! transcript ([`Conversation::written`]): code and markdown the voice left
//! out stay in the model's history (B2 review 2).
//!
//! An assistant audio item the client created has no table until it is
//! truncated: one is made from it then — its transcript as one clause, as
//! long as the audio it carries (none: zero long).

use super::super::audio::pcm::decode_pcm16;
use super::super::audio::resample::INPUT_RATE;
use super::super::heard::{HeardError, HeardTable, Written};
use super::super::protocol::{ContentPart, ErrorObject, Item, MessageItem, Role, ServerEvent};
use super::{not_found, Conversation};

impl Conversation {
    /// A clause of `item_id` was queued: record it and make the item's
    /// transcript what has been queued — appended, as the table joins its
    /// clauses, rather than re-joined: a long answer would otherwise cost
    /// the square of its length (WP3 review M3). `true` for the item's first
    /// clause. `written` is what the model wrote for it.
    pub fn heard_push(
        &mut self,
        item_id: &str,
        text: &str,
        written: Written,
        samples: u64,
    ) -> bool {
        let table = self
            .heard
            .entry(item_id.to_string())
            .or_insert_with(|| HeardTable::new(INPUT_RATE).expect("the output rate is not zero"));
        let first = table.clauses().is_empty();
        table.push_written(text, written, samples);
        let text = text.trim();
        if !text.is_empty() {
            if let Some(t) = self.transcript_mut(item_id) {
                if !t.is_empty() {
                    t.push(' ');
                }
                t.push_str(text);
            }
        }
        first
    }

    /// What the model wrote after `item_id`'s last clause and the voice
    /// left out (a closing code block): its history, not its transcript.
    pub fn heard_unspoken(&mut self, item_id: &str, raw: &str) {
        if let Some(table) = self.heard.get_mut(item_id) {
            table.push_unspoken(raw);
        }
    }

    /// What the model wrote of assistant audio item `item_id`, up to what
    /// was heard — its text in the model's history (module doc). `None`
    /// for an item without a table: its transcript is all there is.
    pub fn written(&self, item_id: &str) -> Option<String> {
        self.heard.get(item_id).map(HeardTable::written)
    }

    /// What the model wrote of assistant item `item_id`, up to what was
    /// heard: its heard table's [`Self::written`], or — a text item, or an
    /// audio item nothing of which was said — the item's own text. What a
    /// bound session's journal cuts a reply to (chat-voice design §8.4).
    pub fn heard_written(&self, item_id: &str) -> String {
        if let Some(w) = self.written(item_id) {
            return w;
        }
        let Some(Item::Message(m)) = self.get(item_id) else {
            return String::new();
        };
        m.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::OutputText { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>()
            .trim()
            .to_string()
    }

    /// [`Self::heard_written`] without the clause the cut fell in
    /// (`HeardTable::written_whole`): always the model's own text, for a
    /// clause said otherwise than written (chat-voice design §8.4).
    pub fn heard_written_whole(&self, item_id: &str) -> String {
        match self.heard.get(item_id) {
            Some(table) => table.written_whole(),
            None => self.heard_written(item_id),
        }
    }

    /// Whether audio was cut away from `item_id` (a truncate, or a cancel
    /// that kept less than it had).
    pub fn heard_clipped(&self, item_id: &str) -> bool {
        self.heard.get(item_id).is_some_and(HeardTable::clipped)
    }

    /// A cancelled item keeps the first `samples` of its audio — what left
    /// for the listener (module doc).
    pub fn heard_keep(&mut self, item_id: &str, samples: u64) {
        let Some(table) = self.heard.get_mut(item_id) else {
            return;
        };
        let transcript = table.keep(samples);
        self.set_transcript(item_id, transcript);
    }

    /// `conversation.item.truncate` (§7.3): cut an assistant audio item at
    /// `audio_end_ms` — whole clauses before it, the clause it falls in to
    /// its last word heard whole, nothing after — and answer
    /// `conversation.item.truncated`.
    /// A cut beyond the item's audio is an error, as with OpenAI, and
    /// changes nothing.
    pub fn truncate(
        &mut self,
        item_id: &str,
        content_index: u32,
        audio_end_ms: u64,
    ) -> Result<ServerEvent, ErrorObject> {
        let item = self.get(item_id).ok_or_else(|| not_found(item_id))?;
        let audio_at = match item {
            Item::Message(MessageItem {
                role: Role::Assistant,
                content,
                ..
            }) => content
                .iter()
                .position(|p| matches!(p, ContentPart::OutputAudio { .. })),
            _ => None,
        };
        let Some(audio_at) = audio_at else {
            return Err(ErrorObject::invalid(
                "invalid_value",
                format!(
                    "item '{item_id}' has no assistant audio to truncate — \
                     conversation.item.truncate cuts an assistant audio item at what the \
                     listener heard; delete or replace a text item instead"
                ),
            )
            .with_param("item_id"));
        };
        if content_index as usize != audio_at {
            return Err(ErrorObject::invalid(
                "invalid_value",
                format!("item '{item_id}' carries its audio at content_index {audio_at}"),
            )
            .with_param("content_index"));
        }
        if !self.heard.contains_key(item_id) {
            let table = table_of(item);
            self.heard.insert(item_id.to_string(), table);
        }
        let table = self.heard.get_mut(item_id).expect("inserted above");
        let transcript = table.truncate(audio_end_ms).map_err(|e| match e {
            HeardError::BeyondEnd { total_ms, .. } => ErrorObject::invalid(
                "invalid_value",
                format!(
                    "audio_end_ms {audio_end_ms} is beyond the audio of item '{item_id}' \
                     ({total_ms} ms)"
                ),
            )
            .with_param("audio_end_ms"),
            HeardError::ZeroRate => ErrorObject::invalid("invalid_value", e.to_string()),
        })?;
        self.set_transcript(item_id, transcript);
        Ok(ServerEvent::ItemTruncated {
            item_id: item_id.to_string(),
            content_index,
            audio_end_ms,
        })
    }

    fn set_transcript(&mut self, item_id: &str, transcript: String) {
        if let Some(t) = self.transcript_mut(item_id) {
            *t = transcript;
        }
    }

    /// The transcript of `item_id`'s audio part.
    fn transcript_mut(&mut self, item_id: &str) -> Option<&mut String> {
        let Some(Item::Message(m)) = self.get_mut(item_id) else {
            return None;
        };
        m.content.iter_mut().find_map(|part| match part {
            ContentPart::OutputAudio { transcript, .. } => {
                Some(transcript.get_or_insert_with(String::new))
            }
            _ => None,
        })
    }
}

/// A client-made audio item's table: its transcript, as long as its audio.
fn table_of(item: &Item) -> HeardTable {
    let mut table = HeardTable::new(INPUT_RATE).expect("the output rate is not zero");
    if let Item::Message(m) = item {
        for part in &m.content {
            if let ContentPart::OutputAudio { audio, transcript } = part {
                let samples = audio
                    .as_deref()
                    .and_then(|a| decode_pcm16(a).ok())
                    .map_or(0, |s| s.len() as u64);
                table.push_clause(transcript.clone().unwrap_or_default(), samples);
            }
        }
    }
    table
}
