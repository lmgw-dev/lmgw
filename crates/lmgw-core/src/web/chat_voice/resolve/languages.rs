//! A thread's two languages (chat-voice design §2.1, split 2026-10-05): the
//! one the user **speaks** — what the ASR is told — and the one replies are
//! in — what the model is asked to answer in and the TTS speaks. The model
//! and the TTS always share the reply language; an owner who speaks German
//! and wants English replies sets both.
//!
//! - **Spoken** ([`language`]): the thread's `voice.language`, then
//!   `chat_voice_language`; none at either is none (the ASR detects). A
//!   thread's `auto` is none from the thread, whatever Settings say.
//! - **Reply** ([`reply_language`]): the thread's `voice.reply_language`,
//!   then `chat_voice_reply_language`, then the spoken language as resolved
//!   (`source: speech_in`) — so a single language set the old way still
//!   drives all three stages, as before the split. A thread's `auto` passes
//!   over Settings' reply language and takes the spoken one as resolved
//!   (its own, Settings', or none); none at all is none: the reply follows
//!   the user.

use crate::config::{Settings, Snapshot};
use crate::store::{ChatThread, ThreadVoice, FOLLOW_USER};
use crate::web::chat_turn::TurnLanguage;

use super::{first, Source, Sourced};

/// The language the user speaks in `v`'s thread (module doc).
pub(crate) fn language(s: &Settings, v: &ThreadVoice) -> Sourced<Option<String>> {
    match first([
        (v.language.as_deref(), Source::Thread),
        (Some(s.chat_voice_language.as_str()), Source::Chat),
    ]) {
        Some((l, src)) if src == Source::Thread && l == FOLLOW_USER => Sourced {
            value: None,
            source: Some(src),
        },
        Some((l, src)) => Sourced {
            value: Some(l.to_string()),
            source: Some(src),
        },
        None => Sourced {
            value: None,
            source: None,
        },
    }
}

/// The language replies in `v`'s thread are in (module doc), `spoken`
/// being its spoken language as [`language`] resolved it.
pub(crate) fn reply_language(
    s: &Settings,
    v: &ThreadVoice,
    spoken: &Sourced<Option<String>>,
) -> Sourced<Option<String>> {
    let follows = || Sourced {
        value: spoken.value.clone(),
        source: spoken.value.is_some().then_some(Source::SpeechIn),
    };
    let own = first([(v.reply_language.as_deref(), Source::Thread)]);
    let set = match own {
        Some((l, _)) if l == FOLLOW_USER => return follows(),
        Some(own) => Some(own),
        None => first([(Some(s.chat_voice_reply_language.as_str()), Source::Chat)]),
    };
    match set {
        Some((l, src)) => Sourced {
            value: Some(l.to_string()),
            source: Some(src),
        },
        None => follows(),
    }
}

/// The languages `thread`'s turn goes by (chat-voice design §8.5): the
/// reply's, which the turn is asked to answer in, and the spoken one the
/// prompt may state; `spoken` when the reply is heard. `None` without a
/// reply language — and so without a spoken one either.
pub(crate) fn turn_language(
    snap: &Snapshot,
    thread: &ChatThread,
    spoken: bool,
) -> Option<TurnLanguage> {
    let speaks = language(&snap.settings, &thread.voice);
    let reply = reply_language(&snap.settings, &thread.voice, &speaks).value?;
    Some(TurnLanguage {
        reply,
        speaks: speaks.value,
        spoken,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(
        chat: (&str, &str),
        thread: (Option<&str>, Option<&str>),
    ) -> [Sourced<Option<String>>; 2] {
        let s = Settings {
            chat_voice_language: chat.0.into(),
            chat_voice_reply_language: chat.1.into(),
            ..Default::default()
        };
        let v = ThreadVoice {
            language: thread.0.map(str::to_string),
            reply_language: thread.1.map(str::to_string),
            ..Default::default()
        };
        let spoken = language(&s, &v);
        let reply = reply_language(&s, &v, &spoken);
        [spoken, reply]
    }

    fn sourced(value: Option<&str>, source: Option<Source>) -> Sourced<Option<String>> {
        Sourced {
            value: value.map(str::to_string),
            source,
        }
    }

    #[test]
    fn an_unset_reply_language_follows_the_spoken_one() {
        let [spoken, reply] = resolved(("de", ""), (None, None));
        assert_eq!(spoken, sourced(Some("de"), Some(Source::Chat)));
        assert_eq!(reply, sourced(Some("de"), Some(Source::SpeechIn)));
        let [_, reply] = resolved(("de", ""), (Some("fr"), None));
        assert_eq!(reply, sourced(Some("fr"), Some(Source::SpeechIn)));
        let [spoken, reply] = resolved(("", ""), (None, None));
        assert_eq!(spoken, sourced(None, None));
        assert_eq!(
            reply,
            sourced(None, None),
            "neither: the reply follows the user"
        );
    }

    #[test]
    fn a_set_reply_language_wins_thread_first() {
        let [spoken, reply] = resolved(("de", "en"), (None, None));
        assert_eq!(spoken, sourced(Some("de"), Some(Source::Chat)));
        assert_eq!(reply, sourced(Some("en"), Some(Source::Chat)));
        let [_, reply] = resolved(("de", "en"), (None, Some("fr")));
        assert_eq!(reply, sourced(Some("fr"), Some(Source::Thread)));
        let [spoken, reply] = resolved(("", "en"), (None, None));
        assert_eq!(spoken, sourced(None, None), "the ASR still detects");
        assert_eq!(reply, sourced(Some("en"), Some(Source::Chat)));
    }

    #[test]
    fn auto_on_each_field() {
        // Spoken `auto`: none from the thread; a reply set in Settings stays.
        let [spoken, reply] = resolved(("de", "en"), (Some(FOLLOW_USER), None));
        assert_eq!(spoken, sourced(None, Some(Source::Thread)));
        assert_eq!(reply, sourced(Some("en"), Some(Source::Chat)));
        // …and with no reply language anywhere, the reply follows the user.
        let [_, reply] = resolved(("de", ""), (Some(FOLLOW_USER), None));
        assert_eq!(reply, sourced(None, None));
        // Reply `auto`: Settings' reply language passed over for the
        // spoken one, the thread's own or Settings'.
        let [_, reply] = resolved(("de", "en"), (None, Some(FOLLOW_USER)));
        assert_eq!(reply, sourced(Some("de"), Some(Source::SpeechIn)));
        let [_, reply] = resolved(("de", "en"), (Some("it"), Some(FOLLOW_USER)));
        assert_eq!(reply, sourced(Some("it"), Some(Source::SpeechIn)));
        // Both `auto`: none at all, whatever Settings say.
        let [spoken, reply] = resolved(("de", "en"), (Some(FOLLOW_USER), Some(FOLLOW_USER)));
        assert_eq!(spoken.value, None);
        assert_eq!(reply, sourced(None, None));
    }
}
