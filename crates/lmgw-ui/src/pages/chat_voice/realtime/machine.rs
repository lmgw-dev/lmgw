//! The panel's state (chat-voice §9.3) and its captions line (§9.1), as pure
//! rules tested natively.
//!
//! The state is **derived**, not stepped: the events set a few facts — the
//! user speaks (server VAD, or push-to-talk held), a turn waits for its
//! response, responses are open, a response's audio is in the player, a cut
//! was just made, a tool runs, the player ran dry — and [`Machine::state`]
//! reads them in one order: interrupted (for [`INTERRUPTED_MS`] after a cut
//! that cut heard audio), listening, speaking, thinking, idle. So an event
//! that comes late or twice cannot leave the panel in a state nothing will
//! leave.
//!
//! **A tool turn is one audio item** (§8.5, WP9 review m3): its spoken
//! preamble ("Moment.") makes it speak, and the item stays open while the
//! tool runs. Once the player has played what it had (an underrun of that
//! item) while a tool runs, the voice is silent: the state is `thinking`
//! until audio of it comes again. An underrun with no tool running (a slow
//! clause between two others) keeps `speaking`: the reply is mid-sentence.

/// How long `interrupted` shows after a barge-in or a stop that cut heard
/// audio (visual only), before `listening` or `idle`.
pub(crate) const INTERRUPTED_MS: f64 = 600.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum VoiceState {
    #[default]
    Idle,
    Listening,
    Thinking,
    Speaking,
    Interrupted,
}

impl VoiceState {
    /// The wire name: `data-voice-state`, and the visualisation's
    /// `setState` (§10).
    pub(crate) fn key(self) -> &'static str {
        match self {
            VoiceState::Idle => "idle",
            VoiceState::Listening => "listening",
            VoiceState::Thinking => "thinking",
            VoiceState::Speaking => "speaking",
            VoiceState::Interrupted => "interrupted",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            VoiceState::Idle => "Idle",
            VoiceState::Listening => "Listening",
            VoiceState::Thinking => "Thinking",
            VoiceState::Speaking => "Speaking",
            VoiceState::Interrupted => "Interrupted",
        }
    }
}

/// The facts the state is read from.
#[derive(Debug, Clone, Default)]
pub(crate) struct Machine {
    /// The server heard speech start and not stop yet, or push-to-talk is
    /// held.
    user: bool,
    /// A turn was committed (or its speech stopped) and no response has
    /// been created for it yet.
    awaiting: bool,
    /// Responses created and not done, oldest first.
    open: Vec<String>,
    /// The response whose audio is in the player.
    playing: Option<String>,
    /// The response whose audio ran dry in the player before its end (an
    /// underrun), until audio of it comes again.
    starved: Option<String>,
    /// A turn's tool runs (its `start` came, its `result` not yet).
    tool: bool,
    /// `interrupted` shows until then (the page's clock, ms).
    cut_until: Option<f64>,
}

impl Machine {
    pub(crate) fn state(&self, now_ms: f64) -> VoiceState {
        if self.cut_until.is_some_and(|t| now_ms < t) {
            VoiceState::Interrupted
        } else if self.user {
            VoiceState::Listening
        } else if self.playing.is_some() && !(self.tool && self.starved == self.playing) {
            VoiceState::Speaking
        } else if self.awaiting || !self.open.is_empty() {
            VoiceState::Thinking
        } else {
            VoiceState::Idle
        }
    }

    /// When the state may change by itself (the end of `interrupted`).
    pub(crate) fn next_change(&self, now_ms: f64) -> Option<f64> {
        self.cut_until.filter(|t| *t > now_ms)
    }

    pub(crate) fn speech_started(&mut self) {
        self.user = true;
    }

    /// The speech ended: the server commits the turn next (§9.3: the
    /// listening state is left on `speech_stopped`).
    pub(crate) fn speech_stopped(&mut self) {
        self.user = false;
        self.awaiting = true;
    }

    pub(crate) fn committed(&mut self) {
        self.user = false;
        self.awaiting = true;
    }

    /// `input_audio_buffer.cleared`: the open turn and its audio are gone,
    /// so nothing waits for a response (review m4: switching to
    /// push-to-talk mid-utterance ends the turn with `speech_stopped` and no
    /// commit, and the page's clear is answered with this).
    pub(crate) fn cleared(&mut self) {
        self.user = false;
        self.awaiting = false;
    }

    pub(crate) fn ptt_down(&mut self) {
        self.user = true;
    }

    /// Push-to-talk let go; `asked`: a commit and `response.create` went.
    pub(crate) fn ptt_up(&mut self, asked: bool) {
        self.user = false;
        if asked {
            self.awaiting = true;
        }
    }

    pub(crate) fn response_created(&mut self, id: &str) {
        self.awaiting = false;
        if !self.open.iter().any(|o| o == id) {
            self.open.push(id.to_string());
        }
    }

    /// The response's first audio is in the player.
    pub(crate) fn audio_started(&mut self, id: &str) {
        self.playing = Some(id.to_string());
    }

    /// Response `id`'s audio ran dry in the player (`true`), or came again.
    pub(crate) fn starved(&mut self, id: &str, starved: bool) {
        if starved {
            self.starved = Some(id.to_string());
        } else if self.starved.as_deref() == Some(id) {
            self.starved = None;
        }
    }

    /// A turn's tool started (`true`) or gave its result.
    pub(crate) fn set_tool(&mut self, running: bool) {
        self.tool = running;
    }

    /// The response's audio played to its end. Its words are all said: it
    /// no longer counts as thinking while the gateway finishes it (a bound
    /// response is done only once pacing's modelled playback has drained,
    /// which may come after the page's own end — measured live: a blip of
    /// `thinking` between `speaking` and `idle`).
    pub(crate) fn played_out(&mut self, id: &str) {
        if self.playing.as_deref() == Some(id) {
            self.playing = None;
        }
        self.starved(id, false);
        self.open.retain(|o| o != id);
    }

    pub(crate) fn response_done(&mut self, id: &str) {
        self.open.retain(|o| o != id);
    }

    /// A turn that will get no response: its transcription failed, it was
    /// refused (`empty_turn`, …), or nothing was committed.
    pub(crate) fn turn_failed(&mut self) {
        self.awaiting = false;
    }

    /// Response `id`'s audio was flushed (a barge-in, stop talking, another
    /// playback took it). `heard_any`: audio of it had been heard, so the
    /// panel shows `interrupted` for a moment. A late cut of an older
    /// response leaves a newer one playing (review NIT 2).
    pub(crate) fn cut(&mut self, id: &str, now_ms: f64, heard_any: bool) {
        if self.playing.as_deref() == Some(id) {
            self.playing = None;
        }
        self.starved(id, false);
        if heard_any {
            self.cut_until = Some(now_ms + INTERRUPTED_MS);
        }
    }

    /// Is response `id` created and not done?
    pub(crate) fn is_open(&self, id: &str) -> bool {
        self.open.iter().any(|o| o == id)
    }

    /// Is a response's audio in the player now?
    pub(crate) fn playing(&self) -> Option<&str> {
        self.playing.as_deref()
    }

    /// The open response a stop would cancel: the one playing, else the
    /// newest one open.
    pub(crate) fn current(&self) -> Option<&str> {
        self.playing
            .as_deref()
            .or_else(|| self.open.last().map(String::as_str))
    }
}

/// After a stop's truncate (`live.rs`, WP9 review NIT 13): does the
/// response still need a `response.cancel`? Only while it is open and the
/// truncate did not stop it: nothing of it was truncated, or its item was
/// complete already (`audio_done`), so the gateway's truncate cancels
/// nothing. A truncate of an item still being produced cancels the rest of
/// its response in the gateway itself.
pub(crate) fn cancel_after_truncate(open: bool, truncated: bool, audio_done: bool) -> bool {
    open && !(truncated && !audio_done)
}

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
    fn a_turn_walks_idle_listening_thinking_speaking_idle() {
        let mut m = Machine::default();
        assert_eq!(m.state(0.0), VoiceState::Idle);
        m.speech_started();
        assert_eq!(m.state(0.0), VoiceState::Listening);
        m.speech_stopped();
        assert_eq!(m.state(0.0), VoiceState::Thinking);
        m.committed();
        m.response_created("r1");
        assert_eq!(m.state(0.0), VoiceState::Thinking, "ASR, LLM, first clause");
        m.audio_started("r1");
        assert_eq!(m.state(0.0), VoiceState::Speaking);
        // `response.done` comes before the audio has played out.
        m.response_done("r1");
        assert_eq!(m.state(0.0), VoiceState::Speaking);
        m.played_out("r1");
        assert_eq!(m.state(0.0), VoiceState::Idle);
    }

    #[test]
    fn a_reply_played_out_before_its_done_is_idle_not_thinking() {
        let mut m = Machine::default();
        m.response_created("r1");
        m.audio_started("r1");
        m.played_out("r1");
        assert_eq!(m.state(0.0), VoiceState::Idle);
        m.response_done("r1");
        assert_eq!(m.state(0.0), VoiceState::Idle);
    }

    #[test]
    fn a_barge_in_shows_interrupted_then_listening() {
        let mut m = Machine::default();
        m.response_created("r1");
        m.audio_started("r1");
        m.speech_started();
        m.cut("r1", 1000.0, true);
        assert_eq!(m.state(1000.0), VoiceState::Interrupted);
        assert_eq!(m.next_change(1000.0), Some(1000.0 + INTERRUPTED_MS));
        assert_eq!(m.state(1000.0 + INTERRUPTED_MS), VoiceState::Listening);
        assert_eq!(m.next_change(1000.0 + INTERRUPTED_MS), None);
        // The cancelled response is done; the user stops: their turn.
        m.response_done("r1");
        m.speech_stopped();
        assert_eq!(m.state(2000.0), VoiceState::Thinking);
    }

    #[test]
    fn a_stop_that_cut_nothing_heard_does_not_show_interrupted() {
        let mut m = Machine::default();
        m.response_created("r1");
        m.audio_started("r1");
        m.cut("r1", 0.0, false);
        m.response_done("r1");
        assert_eq!(m.state(0.0), VoiceState::Idle);
        // A stop after an interrupted one, the user silent: idle at its end.
        m.response_created("r2");
        m.audio_started("r2");
        m.cut("r2", 10.0, true);
        m.response_done("r2");
        assert_eq!(m.state(10.0), VoiceState::Interrupted);
        assert_eq!(m.state(10.0 + INTERRUPTED_MS), VoiceState::Idle);
    }

    #[test]
    fn push_to_talk_and_a_refused_turn() {
        let mut m = Machine::default();
        m.ptt_down();
        assert_eq!(m.state(0.0), VoiceState::Listening);
        m.ptt_up(true);
        assert_eq!(m.state(0.0), VoiceState::Thinking);
        // `empty_turn`: no response will come.
        m.turn_failed();
        assert_eq!(m.state(0.0), VoiceState::Idle);
        m.ptt_down();
        m.ptt_up(false);
        assert_eq!(m.state(0.0), VoiceState::Idle);
    }

    #[test]
    fn a_tool_running_after_the_preamble_played_is_thinking() {
        let mut m = Machine::default();
        m.response_created("r1");
        m.audio_started("r1");
        m.set_tool(true);
        assert_eq!(
            m.state(0.0),
            VoiceState::Speaking,
            "the preamble still plays"
        );
        m.starved("r1", true);
        assert_eq!(
            m.state(0.0),
            VoiceState::Thinking,
            "played out, the tool runs"
        );
        m.set_tool(false);
        assert_eq!(
            m.state(0.0),
            VoiceState::Speaking,
            "no tool: a gap mid-reply is no thinking"
        );
        m.set_tool(true);
        m.starved("r1", false);
        assert_eq!(
            m.state(0.0),
            VoiceState::Speaking,
            "the answer's audio came"
        );
        // An underrun of another response changes nothing for this one.
        m.starved("r0", true);
        assert_eq!(m.state(0.0), VoiceState::Speaking);
    }

    #[test]
    fn a_turn_ended_by_a_switch_to_push_to_talk_is_idle_once_cleared() {
        let mut m = Machine::default();
        m.speech_started();
        // Turn detection off: the server ends the open turn, no commit.
        m.speech_stopped();
        assert_eq!(m.state(0.0), VoiceState::Thinking);
        m.cleared();
        assert_eq!(m.state(0.0), VoiceState::Idle);
    }

    #[test]
    fn a_late_cut_of_an_older_response_leaves_the_newer_one_playing() {
        let mut m = Machine::default();
        m.response_created("r1");
        m.audio_started("r1");
        m.response_created("r2");
        m.audio_started("r2");
        m.cut("r1", 0.0, false);
        assert_eq!(m.playing(), Some("r2"));
        assert_eq!(m.state(0.0), VoiceState::Speaking);
        m.cut("r2", 0.0, false);
        assert_eq!(m.playing(), None);
    }

    #[test]
    fn a_turn_waiting_for_its_response_has_nothing_to_stop() {
        // Review NIT 8: thinking before `response.created` offers no stop.
        let mut m = Machine::default();
        m.speech_started();
        m.speech_stopped();
        assert_eq!((m.state(0.0), m.current()), (VoiceState::Thinking, None));
        m.response_created("r1");
        assert_eq!(m.current(), Some("r1"));
    }

    #[test]
    fn a_stop_cancels_only_what_its_truncate_did_not_stop() {
        // Producing, truncated: the gateway's truncate stopped it.
        assert!(!cancel_after_truncate(true, true, false));
        // Nothing heard, so nothing truncated: the cancel alone.
        assert!(cancel_after_truncate(true, false, false));
        // Its item complete, still open (pacing drains): the cancel.
        assert!(cancel_after_truncate(true, true, true));
        // Done already: nothing to cancel.
        assert!(!cancel_after_truncate(false, false, false));
        assert!(!cancel_after_truncate(false, true, true));
    }

    #[test]
    fn the_current_response_is_the_one_playing_else_the_newest_open() {
        let mut m = Machine::default();
        assert_eq!(m.current(), None);
        m.response_created("resp_a");
        assert_eq!(m.current(), Some("resp_a"));
        m.audio_started("resp_a");
        m.response_created("resp_b");
        assert_eq!(m.current(), Some("resp_a"));
        assert_eq!(m.playing(), Some("resp_a"));
        // A late `played_out` of another response changes nothing.
        m.played_out("resp_b");
        assert_eq!(m.playing(), Some("resp_a"));
    }

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
