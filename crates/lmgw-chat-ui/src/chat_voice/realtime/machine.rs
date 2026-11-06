//! The panel's captions line (chat-voice §9.1), as pure rules tested
//! natively. The voice state it reads is `lmgw-client`'s
//! ([`lmgw_client::voice`]): the facts, the order they are read in, the
//! truncate-first rule — one set of rules for the dashboard and every
//! other client.

pub(crate) use lmgw_client::voice::{Machine, VoiceState};

/// Who a caption is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Who {
    User,
    Assistant,
}

/// How a caption's words show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    /// Words as they come.
    Live,
    /// Words of a turn that is over (the question while it thinks, a cut
    /// reply).
    Past,
    /// What to do, not words said.
    Hint,
}

/// The one captions line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Caption {
    pub who: Option<Who>,
    pub text: String,
    pub tone: Tone,
    /// A small tag after the words ("interrupted").
    pub tag: Option<&'static str>,
}

impl Default for Caption {
    fn default() -> Self {
        Caption {
            who: None,
            text: String::new(),
            tone: Tone::Hint,
            tag: None,
        }
    }
}

/// The words the captions line draws from: the user's latest turn and the
/// reply being spoken.
#[derive(Debug, Clone, Default)]
pub(crate) struct Captions {
    user: String,
    reply: String,
    reply_id: Option<String>,
}

impl Captions {
    /// A new utterance starts: the last one's words go.
    pub(crate) fn user_started(&mut self) {
        self.user.clear();
    }

    pub(crate) fn user_delta(&mut self, d: &str) {
        self.user.push_str(d);
    }

    pub(crate) fn user_final(&mut self, t: &str) {
        self.user = t.trim().to_string();
    }

    /// The reply's spoken words, paced with its audio (`response_id`'s).
    pub(crate) fn spoken(&mut self, response_id: &str, d: &str) {
        if self.reply_id.as_deref() != Some(response_id) {
            self.reply_id = Some(response_id.to_string());
            self.reply.clear();
        }
        self.reply.push_str(d);
    }

    /// The line for `state`.
    pub(crate) fn line(&self, state: VoiceState, muted: bool, ptt: bool) -> Caption {
        let words = |s: &str| {
            let s = s.trim();
            if s.is_empty() {
                "…".to_string()
            } else {
                s.to_string()
            }
        };
        match state {
            VoiceState::Listening => Caption {
                who: Some(Who::User),
                text: words(&self.user),
                tone: Tone::Live,
                tag: None,
            },
            VoiceState::Thinking => Caption {
                who: Some(Who::User),
                text: words(&self.user),
                tone: Tone::Past,
                tag: None,
            },
            VoiceState::Speaking => Caption {
                who: Some(Who::Assistant),
                text: words(&self.reply),
                tone: Tone::Live,
                tag: None,
            },
            VoiceState::Interrupted => Caption {
                who: Some(Who::Assistant),
                text: words(&self.reply),
                tone: Tone::Past,
                tag: Some("interrupted"),
            },
            VoiceState::Idle => Caption {
                who: None,
                text: if muted {
                    "Microphone muted (M unmutes)"
                } else if ptt {
                    "Hold Space to talk"
                } else {
                    "Listening for your voice …"
                }
                .to_string(),
                tone: Tone::Hint,
                tag: None,
            },
        }
    }

    /// The user's words so far (the probes read them).
    pub(crate) fn user(&self) -> &str {
        &self.user
    }

    pub(crate) fn reply(&self) -> &str {
        &self.reply
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_captions_follow_the_state() {
        let mut c = Captions::default();
        assert_eq!(
            c.line(VoiceState::Idle, false, false).text,
            "Listening for your voice …"
        );
        assert_eq!(
            c.line(VoiceState::Idle, false, true).text,
            "Hold Space to talk"
        );
        assert!(c
            .line(VoiceState::Idle, true, true)
            .text
            .starts_with("Microphone muted"));
        c.user_started();
        assert_eq!(c.line(VoiceState::Listening, false, false).text, "…");
        c.user_final(" Wie spät ist es? ");
        let l = c.line(VoiceState::Thinking, false, false);
        assert_eq!(
            (l.who, l.text.as_str(), l.tone),
            (Some(Who::User), "Wie spät ist es?", Tone::Past)
        );
        c.spoken("r1", "Es ist ");
        c.spoken("r1", "halb neun.");
        let l = c.line(VoiceState::Speaking, false, false);
        assert_eq!(
            (l.who, l.text.as_str()),
            (Some(Who::Assistant), "Es ist halb neun.")
        );
        let l = c.line(VoiceState::Interrupted, false, false);
        assert_eq!((l.tone, l.tag), (Tone::Past, Some("interrupted")));
        // The next response's words start a new line.
        c.spoken("r2", "Gern.");
        assert_eq!(c.reply(), "Gern.");
        c.user_started();
        assert_eq!(c.user(), "");
    }
}
