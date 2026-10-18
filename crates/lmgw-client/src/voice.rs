//! The voice state of a bound session (chat-voice design §9.3), derived
//! from facts, not stepped.
//!
//! The events set a few facts — the user speaks (server VAD, or push-to-talk
//! held), a turn waits for its response, responses are open, a response's
//! audio is in the player, a cut was just made, a tool runs, the player ran
//! dry — and [`Machine::state`] reads them in one order: interrupted (for
//! [`INTERRUPTED_MS`] after a cut that cut heard audio), listening,
//! speaking, thinking, idle. So an event that comes late or twice cannot
//! leave the state somewhere nothing leaves.
//!
//! **A tool turn is one audio item** (§8.5): its spoken preamble ("Moment.")
//! makes it speak, and the item stays open while the tool runs. Once the
//! player has played what it had (an underrun of that item) while a tool
//! runs, the voice is silent: the state is `thinking` until audio of it
//! comes again. An underrun with no tool running (a slow clause between two
//! others) keeps `speaking`: the reply is mid-sentence.
//!
//! **On a WebRTC call** (realtime calls design §6.2, §8.3) the player is
//! libwebrtc's, which the client cannot watch: lmgw says what it does.
//! `output_audio_buffer.started` puts a response's audio in the player
//! ([`Machine::output_started`]), `.stopped` says it played out
//! ([`Machine::output_stopped`]) and `.cleared` that it was cut
//! ([`Machine::output_cleared`]); `lmgw.output_audio.starved` is
//! [`Machine::starved`]. `interrupted` shows for a cut that a barge-in made
//! (its cancelled `response.done` said `turn_detected`,
//! [`Machine::response_cancelled`]). Two facts stand beside the state:
//! - [`Machine::playback_muted`]: the remote track is to be muted, from the
//!   user's stop ([`Machine::stop_tapped`]) or a `cleared` until the next
//!   response's `started`, so what libwebrtc still holds is never heard;
//! - [`Machine::held_by`]: the bind waits (`lmgw.session.bind_pending`)
//!   for the user's choice, and no session runs.
//!
//! **No clock.** Every method that needs the time takes it (`now_ms`, any
//! monotonic milliseconds the client keeps).

/// How long `interrupted` shows after a barge-in or a stop that cut heard
/// audio (visual only), before `listening` or `idle`.
pub const INTERRUPTED_MS: f64 = 600.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoiceState {
    #[default]
    Idle,
    Listening,
    Thinking,
    Speaking,
    Interrupted,
}

impl VoiceState {
    /// The wire name: the visualisation's `setState` (chat-voice design
    /// §10).
    pub fn key(self) -> &'static str {
        match self {
            VoiceState::Idle => "idle",
            VoiceState::Listening => "listening",
            VoiceState::Thinking => "thinking",
            VoiceState::Speaking => "speaking",
            VoiceState::Interrupted => "interrupted",
        }
    }

    pub fn label(self) -> &'static str {
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Machine {
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
    /// `interrupted` shows until then.
    cut_until: Option<f64>,
    /// A call's: the response a barge-in cancelled (`turn_detected`), whose
    /// `cleared` shows `interrupted`.
    barged: Option<String>,
    /// A call's: the remote track is to be muted.
    muted: bool,
    /// The responses being cut when it was muted, whose `started` (one
    /// that crossed the stop) does not lift the mute.
    muted_for: Vec<String>,
    /// A call's bind waits for the user's choice; the holder.
    held_by: Option<String>,
}

impl Machine {
    pub fn state(&self, now_ms: f64) -> VoiceState {
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
    pub fn next_change(&self, now_ms: f64) -> Option<f64> {
        self.cut_until.filter(|t| *t > now_ms)
    }

    pub fn speech_started(&mut self) {
        self.user = true;
    }

    /// The speech ended: the server commits the turn next (§9.3: the
    /// listening state is left on `speech_stopped`).
    pub fn speech_stopped(&mut self) {
        self.user = false;
        self.awaiting = true;
    }

    pub fn committed(&mut self) {
        self.user = false;
        self.awaiting = true;
    }

    /// `input_audio_buffer.cleared`: the open turn and its audio are gone,
    /// so nothing waits for a response (switching to push-to-talk
    /// mid-utterance ends the turn with `speech_stopped` and no commit, and
    /// the client's clear is answered with this).
    pub fn cleared(&mut self) {
        self.user = false;
        self.awaiting = false;
    }

    pub fn ptt_down(&mut self) {
        self.user = true;
    }

    /// Push-to-talk let go; `asked`: a commit and `response.create` went.
    pub fn ptt_up(&mut self, asked: bool) {
        self.user = false;
        if asked {
            self.awaiting = true;
        }
    }

    pub fn response_created(&mut self, id: &str) {
        self.awaiting = false;
        if !self.open.iter().any(|o| o == id) {
            self.open.push(id.to_string());
        }
    }

    /// The response's first audio is in the player.
    pub fn audio_started(&mut self, id: &str) {
        self.playing = Some(id.to_string());
    }

    /// Response `id`'s audio ran dry in the player (`true`), or came again.
    pub fn starved(&mut self, id: &str, starved: bool) {
        if starved {
            self.starved = Some(id.to_string());
        } else if self.starved.as_deref() == Some(id) {
            self.starved = None;
        }
    }

    /// A turn's tool started (`true`) or gave its result.
    pub fn set_tool(&mut self, running: bool) {
        self.tool = running;
    }

    /// The response's audio played to its end. Its words are all said: it
    /// no longer counts as thinking while the gateway finishes it (a bound
    /// response is done only once pacing's modelled playback has drained,
    /// which may come after the client's own end).
    pub fn played_out(&mut self, id: &str) {
        if self.playing.as_deref() == Some(id) {
            self.playing = None;
        }
        self.starved(id, false);
        self.open.retain(|o| o != id);
    }

    pub fn response_done(&mut self, id: &str) {
        self.open.retain(|o| o != id);
    }

    /// A turn that will get no response: its transcription failed, it was
    /// refused (`empty_turn`, …), or nothing was committed.
    pub fn turn_failed(&mut self) {
        self.awaiting = false;
    }

    /// Response `id`'s audio was flushed (a barge-in, a stop, another
    /// playback took the speakers). `heard_any`: audio of it had been
    /// heard, so `interrupted` shows for a moment. A late cut of an older
    /// response leaves a newer one playing.
    pub fn cut(&mut self, id: &str, now_ms: f64, heard_any: bool) {
        if self.playing.as_deref() == Some(id) {
            self.playing = None;
        }
        self.starved(id, false);
        if heard_any {
            self.cut_until = Some(now_ms + INTERRUPTED_MS);
        }
    }

    /// Is response `id` created and not done?
    pub fn is_open(&self, id: &str) -> bool {
        self.open.iter().any(|o| o == id)
    }

    /// The response whose audio is in the player now.
    pub fn playing(&self) -> Option<&str> {
        self.playing.as_deref()
    }

    /// The open response a stop would cancel: the one playing, else the
    /// newest one open.
    pub fn current(&self) -> Option<&str> {
        self.playing
            .as_deref()
            .or_else(|| self.open.last().map(String::as_str))
    }

    /// Nothing is open, waited for or playing, and the user does not speak:
    /// a quiet point, where a client may close or rebind its session
    /// without cutting anything. A pending bind is quiet.
    pub fn quiet(&self) -> bool {
        !self.user && !self.awaiting && self.open.is_empty() && self.playing.is_none()
    }

    /// `lmgw.session.bind_pending {held_by}`: the call is connected and
    /// holds no thread; it waits for the user's choice (take over, or hang
    /// up). Again with a new holder when a third client took the thread.
    pub fn bind_pending(&mut self, held_by: &str) {
        self.held_by = Some(held_by.to_string());
    }

    /// `session.created`: the session runs, bound; a pending bind is over.
    pub fn session_started(&mut self) {
        self.held_by = None;
    }

    /// Who holds the thread while a call's bind waits for the user's
    /// choice; `None` when it does not wait.
    pub fn held_by(&self) -> Option<&str> {
        self.held_by.as_deref()
    }

    /// The cancelled `response.done` of response `id`, with its
    /// `status_details.reason`. `turn_detected` is a barge-in: on a call, its
    /// `output_audio_buffer.cleared`, which follows at once, shows
    /// `interrupted` ([`Self::output_cleared`]); a stop's `client_cancelled`
    /// does not.
    pub fn response_cancelled(&mut self, id: &str, reason: Option<&str>) {
        if reason == Some("turn_detected") {
            self.barged = Some(id.to_string());
        }
        self.response_done(id);
    }

    /// `output_audio_buffer.started`: lmgw sent response `id`'s first frame,
    /// so its audio is in the player; and the mute lifts, unless `id` is a
    /// response that was being cut when it began (a `started` that crossed
    /// the stop), whose audio stays unheard.
    pub fn output_started(&mut self, id: &str) {
        if self.muted && self.muted_for.iter().any(|m| m == id) {
            return;
        }
        self.muted = false;
        self.muted_for.clear();
        self.audio_started(id);
    }

    /// `output_audio_buffer.stopped`: response `id`'s audio played to its
    /// end on the client.
    pub fn output_stopped(&mut self, id: &str) {
        self.played_out(id);
    }

    /// `output_audio_buffer.cleared`: lmgw stopped sending response `id`'s
    /// audio before its end. Mute the remote track until the next
    /// response's `started` ([`Self::playback_muted`]); `interrupted` shows
    /// when a barge-in cut it ([`Self::response_cancelled`]).
    pub fn output_cleared(&mut self, id: &str, now_ms: f64) {
        let barge_in = self.barged.as_deref() == Some(id);
        if barge_in {
            self.barged = None;
        }
        self.cut(id, now_ms, barge_in);
        self.mute(&[id]);
    }

    /// The user's stop on a call (the tap, then `lmgw.response.stop`): mute
    /// the remote track now, before lmgw's events come, until the next
    /// response's `started`.
    pub fn stop_tapped(&mut self) {
        let cut: Vec<String> = self.open.iter().chain(&self.playing).cloned().collect();
        self.mute(&cut);
    }

    /// Whether the client mutes the remote track now: from a stop's tap or
    /// a `cleared` until the next response's `started`. A fact beside the
    /// state: it silences what libwebrtc's jitter buffer still holds, and
    /// the state says what the voice does.
    pub fn playback_muted(&self) -> bool {
        self.muted
    }

    fn mute<S: AsRef<str>>(&mut self, cut: &[S]) {
        self.muted = true;
        for id in cut {
            if !self.muted_for.iter().any(|m| m == id.as_ref()) {
                self.muted_for.push(id.as_ref().to_string());
            }
        }
    }
}

/// The truncate-first rule. A stop sends the truncate first: while the item
/// is still being produced, the gateway's truncate cancels the rest of its
/// response in the same step, so the reply gets the exact cut in one write.
/// After it: does the response still need a `response.cancel`? Only while
/// it is open and the truncate did not stop it — nothing of it was
/// truncated, or its item was complete already (`audio_done`), so the
/// gateway's truncate cancels nothing.
pub fn cancel_after_truncate(open: bool, truncated: bool, audio_done: bool) -> bool {
    open && !(truncated && !audio_done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_walks_idle_listening_thinking_speaking_idle() {
        let mut m = Machine::default();
        assert_eq!(m.state(0.0), VoiceState::Idle);
        assert!(m.quiet());
        m.speech_started();
        assert_eq!(m.state(0.0), VoiceState::Listening);
        assert!(!m.quiet());
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
        assert!(!m.quiet(), "still playing");
        m.played_out("r1");
        assert_eq!(m.state(0.0), VoiceState::Idle);
        assert!(m.quiet());
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
        // Thinking before `response.created` offers no stop.
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
    fn a_call_s_player_facts_come_from_lmgw() {
        let mut m = Machine::default();
        m.response_created("r1");
        assert_eq!(m.state(0.0), VoiceState::Thinking);
        m.output_started("r1");
        assert_eq!(m.state(0.0), VoiceState::Speaking);
        // A tool after the spoken preamble: lmgw's underrun reads as thinking.
        m.set_tool(true);
        m.starved("r1", true);
        assert_eq!(m.state(0.0), VoiceState::Thinking);
        m.starved("r1", false);
        m.set_tool(false);
        // `response.done` waits for drained on a call; `stopped` follows it.
        m.response_done("r1");
        assert_eq!(m.state(0.0), VoiceState::Speaking);
        m.output_stopped("r1");
        assert_eq!(m.state(0.0), VoiceState::Idle);
        assert!(m.quiet());
        assert!(!m.playback_muted());
    }

    #[test]
    fn a_barge_in_s_cleared_shows_interrupted_and_mutes_until_the_next_started() {
        let mut m = Machine::default();
        m.response_created("r1");
        m.output_started("r1");
        m.speech_started();
        // Its cancel, then `cleared` at once.
        m.response_cancelled("r1", Some("turn_detected"));
        m.output_cleared("r1", 1000.0);
        assert_eq!(m.state(1000.0), VoiceState::Interrupted);
        assert!(m.playback_muted(), "what NetEQ holds is not heard");
        assert_eq!(m.playing(), None);
        assert_eq!(m.state(1000.0 + INTERRUPTED_MS), VoiceState::Listening);
        m.speech_stopped();
        m.response_created("r2");
        assert!(m.playback_muted(), "until the next response's audio");
        m.output_started("r2");
        assert!(!m.playback_muted());
        assert_eq!(m.state(2000.0), VoiceState::Speaking);
    }

    #[test]
    fn a_stop_mutes_at_the_tap_and_its_cleared_shows_no_interrupted() {
        let mut m = Machine::default();
        m.response_created("r1");
        m.output_started("r1");
        m.stop_tapped();
        assert!(m.playback_muted(), "at the tap, before lmgw answers");
        m.response_cancelled("r1", Some("client_cancelled"));
        m.output_cleared("r1", 500.0);
        assert_eq!(m.state(500.0), VoiceState::Idle);
        assert!(m.playback_muted());
        // A stop with nothing open mutes too, and the next answer lifts it.
        let mut m = Machine::default();
        m.stop_tapped();
        assert!(m.playback_muted());
        m.response_created("r1");
        m.output_started("r1");
        assert!(!m.playback_muted());
    }

    #[test]
    fn a_started_that_crossed_the_stop_does_not_lift_the_mute() {
        let mut m = Machine::default();
        m.response_created("r1");
        // Thinking: the tap, while lmgw already sent r1's first frame.
        m.stop_tapped();
        m.output_started("r1");
        assert!(m.playback_muted());
        assert_eq!(m.playing(), None, "its audio is unheard");
        m.response_cancelled("r1", Some("client_cancelled"));
        m.output_cleared("r1", 0.0);
        assert_eq!(m.state(0.0), VoiceState::Idle);
        m.response_created("r2");
        m.output_started("r2");
        assert!(!m.playback_muted());
        assert_eq!(m.playing(), Some("r2"));
    }

    #[test]
    fn a_pending_bind_names_the_holder_until_the_session_starts() {
        let mut m = Machine::default();
        assert_eq!(m.held_by(), None);
        m.bind_pending("device 'desktop'");
        assert_eq!(m.held_by(), Some("device 'desktop'"));
        assert_eq!(m.state(0.0), VoiceState::Idle);
        assert!(m.quiet(), "a pending bind counts as quiet");
        // A third client took the thread meanwhile.
        m.bind_pending("the dashboard");
        assert_eq!(m.held_by(), Some("the dashboard"));
        m.session_started();
        assert_eq!(m.held_by(), None);
    }

    #[test]
    fn the_wire_names() {
        let all = [
            VoiceState::Idle,
            VoiceState::Listening,
            VoiceState::Thinking,
            VoiceState::Speaking,
            VoiceState::Interrupted,
        ];
        let keys: Vec<_> = all.iter().map(|s| s.key()).collect();
        assert_eq!(
            keys,
            ["idle", "listening", "thinking", "speaking", "interrupted"]
        );
        assert_eq!(VoiceState::Thinking.label(), "Thinking");
    }
}
