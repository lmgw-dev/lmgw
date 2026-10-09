//! A turn's system message and reasoning (personality-profiles design §2.1,
//! §2.2, the reasoning half of §2.3).
//!
//! The parts, with `P` the thread's profile:
//! - **base:** `P.persona` when non-empty, else the thread's
//!   `system_prompt`;
//! - **length:** `P.length_rule`, when non-empty, its own paragraph;
//! - **examples:** when `P.examples` is non-empty, its own paragraph:
//!   [`EXAMPLES_HEADING`], then `User:` / `You:` pairs separated by blank
//!   lines — in the system message, never as turns of the conversation
//!   (D5);
//! - **the static part:** base, length and examples, each trimmed and
//!   expanded (`{{model}}`, `{{date}}`), joined by blank lines. Nothing in
//!   it depends on the turn but the date, which changes daily, so it leads
//!   the system message and the prompt cache keeps it (§2.4).
//!
//! After it, by kind of turn (§2.2), what `chat_voice::prompt::turn_block`
//! adds: a voice turn's block (`P.voice_block` over
//! `realtime.default_instructions`, D6) with the language and the tag hint;
//! for a text turn with a reply language, the language sentence alone. An
//! Admin Chat thread's system message is the admin wrapper around all of
//! that (D22).
//!
//! The one function both a send and the editor's preview build with is
//! [`system_message`].

use std::borrow::Cow;

use chrono::NaiveDate;

use super::super::agentchat::{self, ADMIN_KIND};
use super::super::chat_reasoning;
use super::super::chat_turn::{TurnLanguage, VoiceTurn};
use super::super::chat_voice::{turn_block, with_block};
use crate::config::chat_profile::{ChatProfile, Example, ProfileDraft, Reasoning};
use crate::config::{expand_chat_prompt, RealtimeSettings, Snapshot};
use crate::ir::ReasoningControl;
use crate::store::ChatThread;

/// What opens the examples' paragraph.
pub(crate) const EXAMPLES_HEADING: &str = "Examples of how you answer:";

/// The parts of a profile a turn's prompt and reasoning take, borrowed from
/// a stored profile or from an unsaved draft (the editor's preview).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Profile<'a> {
    /// Empty: none (the thread's prompt says who the model is).
    pub persona: &'a str,
    /// Empty: none.
    pub length_rule: &'a str,
    pub examples: &'a [Example],
    /// `None`: the generic voice block; `Some("")`: none; a text: verbatim.
    pub voice_block: Option<&'a str>,
    /// `None`: inherit.
    pub reasoning: Option<Reasoning>,
}

impl<'a> From<&'a ChatProfile> for Profile<'a> {
    fn from(p: &'a ChatProfile) -> Self {
        Self {
            persona: &p.persona,
            length_rule: &p.length_rule,
            examples: &p.examples,
            voice_block: p.voice_block.as_deref(),
            reasoning: p.reasoning,
        }
    }
}

impl<'a> From<&'a ProfileDraft> for Profile<'a> {
    fn from(p: &'a ProfileDraft) -> Self {
        Self {
            persona: &p.persona,
            length_rule: &p.length_rule,
            examples: &p.examples,
            voice_block: p.voice_block.as_deref(),
            reasoning: p.reasoning,
        }
    }
}

/// What one turn's prompt is built from: the thread, its profile, the alias
/// `{{model}}` names (the one that answers) and the day `{{date}}` names.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Prompt<'a> {
    pub thread: &'a ChatThread,
    pub profile: Option<Profile<'a>>,
    pub model: &'a str,
    pub today: NaiveDate,
}

impl<'a> Prompt<'a> {
    /// `thread`'s turn with the profile its `profile_id` names in `snap` —
    /// none when it names none, or a row that is gone (§2.1).
    pub(crate) fn of(
        snap: &'a Snapshot,
        thread: &'a ChatThread,
        model: &'a str,
        today: NaiveDate,
    ) -> Self {
        Self {
            thread,
            profile: thread
                .profile_id
                .and_then(|id| snap.chat_profile(id))
                .map(Profile::from),
            model,
            today,
        }
    }

    fn expand(&self, text: &str) -> String {
        expand_chat_prompt(text.trim(), self.model, self.today)
    }

    /// Who the model is, as written (unexpanded — its `{{date}}` decides a
    /// voice block's "no date" sentence): the profile's persona when it has
    /// one (D3), else the thread's prompt.
    pub(crate) fn base(&self) -> &'a str {
        match self.profile {
            Some(p) if !p.persona.trim().is_empty() => p.persona,
            _ => &self.thread.system_prompt,
        }
    }

    /// The static part (module doc), expanded; empty when every part is.
    /// With no profile it is the thread's prompt, expanded, as it always
    /// was.
    pub(crate) fn static_part(&self) -> String {
        let mut parts = vec![self.expand(self.base())];
        if let Some(p) = self.profile {
            parts.push(self.expand(p.length_rule));
            parts.push(self.examples(p.examples));
        }
        parts.retain(|s| !s.is_empty());
        parts.join("\n\n")
    }

    /// The examples' paragraph (D5), empty without any.
    fn examples(&self, examples: &[Example]) -> String {
        if examples.is_empty() {
            return String::new();
        }
        let pairs: Vec<String> = examples
            .iter()
            .map(|e| {
                format!(
                    "User: {}\nYou: {}",
                    self.expand(&e.user),
                    self.expand(&e.reply)
                )
            })
            .collect();
        format!("{EXAMPLES_HEADING}\n{}", pairs.join("\n\n"))
    }

    /// A voice turn's instructions (§2.2, D6): the profile's voice block
    /// when it sets one — `""` none, a text verbatim but for its
    /// placeholders — else `realtime.default_instructions` (`None`: the
    /// built-in text).
    pub(crate) fn voice_instructions<'s>(
        &self,
        settings: &'s RealtimeSettings,
    ) -> Option<Cow<'s, str>> {
        match self.profile.and_then(|p| p.voice_block) {
            Some(own) => Some(Cow::Owned(self.expand(own))),
            None => settings.default_instructions.as_deref().map(Cow::Borrowed),
        }
    }

    /// The reasoning a turn asks for before a voice turn's default off
    /// (D7): the thread's own fields, else the profile's on/off.
    pub(crate) fn reasoning(&self) -> Option<ReasoningControl> {
        chat_reasoning::control_with(self.thread, self.profile.and_then(|p| p.reasoning))
    }
}

/// A turn's system message and the reasoning it asks for (§2.2): the static
/// part, then what the kind of turn adds (`voice`: a voice turn, with its
/// TTS's hint; `language`: the turn's languages), the admin wrapper around
/// it on an Admin Chat thread. An empty text is no system message.
pub(crate) fn system_message(
    prompt: &Prompt<'_>,
    settings: &RealtimeSettings,
    voice: Option<&VoiceTurn>,
    language: Option<&TurnLanguage>,
) -> (String, Option<ReasoningControl>) {
    let (block, reasoning) = turn_block(prompt, settings, voice, language);
    let sys = prompt.static_part();
    let sys = match block {
        Some(block) => with_block(sys, &block),
        None => sys,
    };
    let sys = if prompt.thread.kind == ADMIN_KIND {
        agentchat::system_prompt(&sys)
    } else {
        sys
    };
    (sys, reasoning)
}

#[cfg(test)]
mod tests;
