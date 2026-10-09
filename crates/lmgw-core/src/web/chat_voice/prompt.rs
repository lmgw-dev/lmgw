//! The system prompt of a voice turn (chat-voice design §8.5, ruling 5): a
//! turn of a realtime session bound to the thread, with audio output, adds
//! realtime's spoken-style instructions and its tag hint to the thread's
//! own prompt. Built per turn, never stored in `system_prompt`; a
//! text-output session's turns and a `speak: true` text turn do not get it.
//!
//! In order:
//! 1. the thread's prompt, expanded as in a text turn — with a personality
//!    profile, the static part instead (`chat_profile`, personality-profiles
//!    design §2.1): the persona in the prompt's place when it has one, then
//!    the length rule and the examples;
//! 2. the voice block — opened, when the part before it is not empty, by
//!    the bridge ([`BRIDGE`]), then the voice instructions: the profile's
//!    voice block when it sets one (D6), else `realtime.default_instructions`:
//!    - unset: the built-in text without its persona sentence (the prompt
//!      says who the model is), and with its "no date" sentence only while
//!      the base — the persona, else the thread's prompt — has no
//!      `{{date}}`;
//!    - set: that text, verbatim — the owner's words;
//!    - `""`: no voice block, and no bridge either;
//! 3. the language sentence ([`language_sentence`]), when the thread has a
//!    reply language (changed 2026-10-04; since 2026-10-05 it names the
//!    reply language and, apart, the one the user speaks): inside the
//!    built-in text in place of its "answer in the language the user
//!    speaks", else as its own paragraph — a setting, not part of the
//!    owner's wording;
//! 4. the tag/cue hint for the speaking TTS, its own paragraph, when
//!    `realtime.tag_hint` is on.
//!
//! The language goes to every turn of a bound session — a text-output one
//! too: the user spoke — and to a `speak: true` turn, whose reply is
//! spoken; such a turn gets it alone, as its prompt's last paragraph
//! ([`turn_block`]). Without a reply language — and so without a spoken one,
//! which it falls back to — nothing changes: the reply follows the user, as
//! before.
//!
//! The voice block wins on form (length, no Markdown, lists, tables or code,
//! the language); the thread's prompt wins on identity and facts.

use crate::config::{RealtimeSettings, CHAT_PROMPT_DATE, VOICE_FORM, VOICE_NO_DATE, VOICE_STYLE};
use crate::ir::ReasoningControl;

use super::super::chat_profile::Prompt;
use super::super::chat_turn::{TurnLanguage, VoiceTurn};

/// What opens the voice block after a thread prompt (module doc).
pub(crate) const BRIDGE: &str = "This reply is spoken aloud. For it, the instructions below \
                                 replace any guidance above about formatting, Markdown, tables \
                                 and code.";

/// What the model is told about the turn's languages.
mod sentence;
pub(crate) use sentence::language_sentence;

/// What follows the static part in a voice turn's system message: the
/// voice block, the turn's `language` (its reply heard: a voice turn's
/// reply is spoken) and the hint, as paragraphs; empty when there is none
/// of them. `base` is who the model is, as written (its `{{date}}` decides
/// the "no date" sentence); `opened`, whether anything comes before the
/// block (the bridge opens it then); `instructions`, the voice text that
/// applies (`None`: the built-in one, `""`: none).
pub(crate) fn voice_block(
    base: &str,
    opened: bool,
    instructions: Option<&str>,
    hint: Option<&str>,
    language: Option<&TurnLanguage>,
) -> String {
    let language = language
        .filter(|l| !l.reply.trim().is_empty())
        .map(|l| language_sentence(l.speaks.as_deref(), &l.reply, true));
    let mut own_paragraph = language.clone();
    let voice = match instructions {
        None => {
            let style = match own_paragraph.take() {
                Some(l) => format!("{VOICE_FORM} {l}"),
                None => VOICE_STYLE.to_string(),
            };
            Some(if base.contains(CHAT_PROMPT_DATE) {
                style
            } else {
                format!("{style} {VOICE_NO_DATE}")
            })
        }
        Some(own) if own.trim().is_empty() => None,
        Some(own) => Some(own.trim().to_string()),
    };
    let mut paragraphs = Vec::new();
    if let Some(v) = voice {
        paragraphs.push(if opened { format!("{BRIDGE} {v}") } else { v });
    }
    paragraphs.extend(own_paragraph);
    paragraphs.extend(
        hint.map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_string),
    );
    paragraphs.join("\n\n")
}

/// What a voice turn adds to `prompt`'s request (§8.5): the block after
/// its static part ([`voice_block`], `hint` the speaking TTS's, `language`
/// the thread's languages), and the reasoning it asks for — the thread's
/// own when it sets any reasoning field (an effort or a budget alone
/// counts), else its profile's on/off, otherwise off, through the existing
/// control: thinking delays the first word, and is never spoken.
pub(crate) fn voice_request(
    prompt: &Prompt<'_>,
    settings: &RealtimeSettings,
    hint: Option<&str>,
    language: Option<&TurnLanguage>,
) -> (String, Option<ReasoningControl>) {
    let block = voice_block(
        prompt.base(),
        !prompt.static_part().is_empty(),
        prompt.voice_instructions(settings).as_deref(),
        hint,
        language,
    );
    let reasoning = prompt.reasoning().or(Some(ReasoningControl {
        enabled: Some(false),
        ..Default::default()
    }));
    (block, reasoning)
}

/// What a turn adds after `prompt`'s static part, and its reasoning: a
/// voice turn's block ([`voice_request`]); for any other turn with a reply
/// `language`, its sentence alone (module doc), with the thread's own
/// reasoning, else its profile's; otherwise nothing.
pub(crate) fn turn_block(
    prompt: &Prompt<'_>,
    settings: &RealtimeSettings,
    voice: Option<&VoiceTurn>,
    language: Option<&TurnLanguage>,
) -> (Option<String>, Option<ReasoningControl>) {
    match voice {
        Some(v) => {
            let (block, reasoning) = voice_request(prompt, settings, v.hint.as_deref(), language);
            (Some(block), reasoning)
        }
        None => (
            language.map(|l| language_sentence(l.speaks.as_deref(), &l.reply, l.spoken)),
            prompt.reasoning(),
        ),
    }
}

/// The static part (`sys`, expanded) with `block` after it.
pub(crate) fn with_block(sys: String, block: &str) -> String {
    match (sys.is_empty(), block.is_empty()) {
        (_, true) => sys,
        (true, false) => block.to_string(),
        (false, false) => format!("{sys}\n\n{block}"),
    }
}

#[cfg(test)]
mod tests;
