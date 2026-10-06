//! The timing line (realtime design §11): one INFO line per response, with
//! the fields end of turn → commit, ASR, the chat model's first token, the
//! first clause, TTS to first audio — and the total.
//!
//! A turn that `semantic_vad`'s rule ended says which part of the rule did
//! it, beside its end of turn → commit (§6.3): the threshold, the floor
//! window, the maximum wait, or the plain silence window for a pause that
//! could not be scored.
//!
//! Every moment is the core's own clock reading when it learnt of it: the
//! turn's last speech when its audio was processed, the commit, the
//! transcript in hand, the model call's start, and what the responder
//! reported ([`Mark`]s for the first token and the first clause of a spoken
//! answer, the first synthesized clause for its audio). The core hears of a
//! responder's moment a channel hop later — microseconds, not the
//! milliseconds the line is about.
//!
//! Each stage is measured from the one before it, so the parts add up: the
//! chat model's first token from the call's start (the per-call policy
//! check, the gate and the prefill), the first clause from the first token,
//! the first audio from the first clause. A text response has the subset
//! that applies, and so does one that was never answered by speech (a
//! client's `response.create` over typed text: no turn), a manual commit (no
//! detected end of turn) or one that ended early (no first token).
//!
//! **Only its own turn** (package A review #7). A response's turn is the
//! latest committed turn it renders and answers: taken when it is created,
//! and replaced at its launch by any turn committed meanwhile (it renders
//! every committed turn). A turn no response will answer — noise, a failed
//! transcription — is dropped when that is decided, so a later response
//! (typed text, say) is timed from its own `response.created`, not from an
//! old turn's end of speech with the idle time in between: by the owed
//! response's decision, or — for a turn nothing is owed to, with
//! `create_response` off or committed by the client — when its transcript
//! comes in without words (A2 review 5).
//!
//! **A response that waited for MCP listings** (realtime-server-tools
//! §1.2) says how long, as `MCP listing N ms` (`tools_list_ms`): from its
//! creation, or the listing that started while it waited, to the last
//! listing's end — or to its own end, for one cancelled while it waited.
//!
//! **A response that ran server-side calls** (realtime-server-tools §2.4)
//! says how long they ran and how many, as `MCP tools N ms (k calls)`
//! (`tools_ms`, `tool_calls`): from the first call's start to the last
//! one's end, as the responder clocked them — in speech mode the core
//! hears of them behind the audio, which is not their run time. A call
//! refused unrun (its arguments no JSON object) is no call.
//!
//! **"First … at" is what reached the client**: the first text delta or
//! call of a text response; the first audio or call of a spoken one, whose
//! text goes out only with its audio. A call is named for what it is: a
//! client's `function call`, or a server-side `MCP call`. A spoken response
//! that produced tokens but no audio (cancelled first) says so, rather than
//! naming a first text the client never got.
//!
//! **A held response** (voice-audio-input design §3.2, `held`) takes its
//! arrival marks as its output arrives — the first token, a clause's
//! synthesis — so `first_token_ms`, `first_clause_ms` and `first_audio_ms`
//! are not inflated by the hold; what reached the client counts at the
//! later of arrival and release, so the wait counts once, in
//! `transcript_wait_ms`: from the first output's arrival to the release (0
//! when nothing waited). The line says `input=audio held=N ms`.
//!
//! [`Mark`]: super::super::responder::Mark

use std::fmt::Write as _;
use std::time::Instant;

use super::super::protocol::ResponseStatus;
use super::super::responder::Mark;
use super::super::turn::semantic::TurnEnd;
use super::{Active, Core};

/// A committed turn's moments, for the response that answers it.
#[derive(Debug, Clone)]
pub(crate) struct TurnTiming {
    pub item_id: String,
    /// When the turn's last speech was heard — its audio processed. `None`
    /// for a client's commit: no end of turn was detected.
    pub speech_end: Option<Instant>,
    pub committed: Instant,
    /// When its transcript was in hand.
    pub transcribed: Option<Instant>,
    /// Which part of `semantic_vad`'s rule ended the turn (§6.3).
    pub ended_by: Option<TurnEnd>,
}

/// One response's moments.
#[derive(Debug)]
pub(super) struct Timing {
    /// The latest committed turn it answers, if any.
    pub turn: Option<TurnTiming>,
    /// It speaks: its text reaches the client only with its audio.
    speaks: bool,
    created: Instant,
    /// The model call's start (`launch`).
    pub launched: Option<Instant>,
    first_token: Option<Instant>,
    /// The chat stream's first reasoning, never spoken (chat-voice §8.5).
    first_reasoning: Option<Instant>,
    /// The first text delta the core sent the client.
    first_text: Option<Instant>,
    /// The first call the core announced to the client, and what it was
    /// (module doc).
    first_call: Option<(Instant, &'static str)>,
    first_clause: Option<Instant>,
    first_audio: Option<Instant>,
    /// It heard the user's audio and started held (`held`): when.
    pub held_at: Option<Instant>,
    /// When its hold was released; `None` while held, or never.
    pub released: Option<Instant>,
    /// When its first output arrived while it was held.
    held_first: Option<Instant>,
    /// It waited for the session's MCP listings: since when, and when the
    /// last of them was in (module doc).
    tools_wait: Option<Instant>,
    tools_listed: Option<Instant>,
    /// Its server-side calls (module doc): the first start, the last end,
    /// and how many ran.
    tools_from: Option<Instant>,
    tools_to: Option<Instant>,
    tool_calls: u32,
}

impl Timing {
    pub fn new(turn: Option<TurnTiming>, speaks: bool) -> Self {
        Self {
            turn,
            speaks,
            created: Instant::now(),
            launched: None,
            first_token: None,
            first_reasoning: None,
            first_text: None,
            first_call: None,
            first_clause: None,
            first_audio: None,
            held_at: None,
            released: None,
            held_first: None,
            tools_wait: None,
            tools_listed: None,
            tools_from: None,
            tools_to: None,
            tool_calls: 0,
        }
    }

    /// Output arrived while the response was held: the first is what its
    /// wait is measured from.
    pub fn held_output(&mut self) {
        self.held_first.get_or_insert_with(Instant::now);
    }

    /// A delta that reached the listener: the first is the first token.
    pub fn token(&mut self) {
        self.first_token.get_or_insert_with(Instant::now);
    }

    /// A text delta went to the client.
    pub fn text(&mut self) {
        self.token();
        self.first_text.get_or_insert_with(Instant::now);
    }

    /// A call was announced to the client: a server-side one of an MCP
    /// tool (`mcp`), or a client's function call.
    pub fn call(&mut self, mcp: bool) {
        self.token();
        let what = if mcp { "MCP call" } else { "function call" };
        self.first_call.get_or_insert((Instant::now(), what));
    }

    /// A server-side call started `at`.
    pub fn tool_running(&mut self, at: Instant) {
        self.tool_calls += 1;
        self.tools_from = Some(self.tools_from.map_or(at, |f| f.min(at)));
    }

    /// A server-side call ended `at`.
    pub fn tool_done(&mut self, at: Instant) {
        self.tools_to = Some(self.tools_to.map_or(at, |t| t.max(at)));
    }

    /// A synthesized clause arrived: the first is the first audio.
    pub fn audio(&mut self) {
        self.first_audio.get_or_insert_with(Instant::now);
    }

    pub fn mark(&mut self, m: Mark) {
        match m {
            Mark::FirstToken => self.token(),
            Mark::FirstClause => {
                self.first_clause.get_or_insert_with(Instant::now);
            }
            Mark::Reasoning => {
                self.first_reasoning.get_or_insert_with(Instant::now);
            }
        }
    }

    /// The response's stages as numbers, measured to `end`: what the line
    /// says, and what a bound session's `lmgw.response.timing` carries
    /// (chat-voice design §8.7) — one computation, so the two never
    /// disagree.
    pub(super) fn stages(&self, end: Instant) -> Stages {
        let ms = |from: Instant, to: Instant| to.saturating_duration_since(from).as_millis() as u64;
        let turn = self.turn.as_ref();
        let (start, from) = match turn {
            Some(TurnTiming {
                speech_end: Some(s),
                ..
            }) => (*s, "the end of speech"),
            Some(t) => (t.committed, "the commit"),
            None => (self.created, "response.created"),
        };
        // What reached the client first (module doc).
        let output = if self.speaks {
            ("audio", self.first_audio)
        } else {
            ("text", self.first_text)
        };
        // A held response's output reached the client at its release at the
        // earliest, and none of it did while it was never released.
        let reached = |at: Instant| self.released.map_or(at, |r| at.max(r));
        let unreleased = self.held_at.is_some() && self.released.is_none();
        let call = self
            .first_call
            .map_or(("function call", None), |(at, what)| (what, Some(at)));
        let first = [output, call]
            .into_iter()
            .filter_map(|(what, at)| Some((what, reached(at?))))
            .filter(|_| !unreleased)
            .min_by_key(|&(_, at)| at)
            .map(|(what, at)| (what, ms(start, at)));
        Stages {
            end_of_turn_ms: turn.and_then(|t| Some(ms(t.speech_end?, t.committed))),
            asr_ms: turn.and_then(|t| Some(ms(t.committed, t.transcribed?))),
            tools_list_ms: self
                .tools_wait
                .map(|w| ms(w, self.tools_listed.unwrap_or(end))),
            first_token_ms: self.launched.zip(self.first_token).map(|(l, f)| ms(l, f)),
            // Part of the first token's wait: from the first reasoning to the
            // first token, or to the end when no token came.
            reasoning_ms: self
                .first_reasoning
                .map(|r| ms(r, self.first_token.filter(|t| *t >= r).unwrap_or(end))),
            first_clause_ms: self
                .first_token
                .zip(self.first_clause)
                .map(|(f, c)| ms(f, c)),
            first_audio_ms: self
                .first_clause
                .zip(self.first_audio)
                .map(|(c, a)| ms(c, a)),
            // To the response's end for calls a cancel cut off.
            tools_ms: self
                .tools_from
                .map(|f| ms(f, self.tools_to.filter(|t| *t >= f).unwrap_or(end))),
            tool_calls: self.tool_calls,
            first,
            no_audio: first.is_none() && self.speaks && self.first_token.is_some(),
            total_ms: ms(start, end),
            from,
            transcript_wait_ms: self.held_at.and(self.released).map(|r| {
                self.held_first
                    .map_or(0, |f| r.saturating_duration_since(f).as_millis() as u64)
            }),
            held: self.held_at.is_some(),
        }
    }

    /// The line's fields, for a response that ended at `end` with `status`.
    pub(super) fn line(&self, status: ResponseStatus, end: Instant) -> String {
        let st = self.stages(end);
        let mut parts: Vec<String> = Vec::new();
        if let Some(ms) = st.end_of_turn_ms {
            let by = self
                .turn
                .as_ref()
                .and_then(|t| t.ended_by)
                .map_or(String::new(), |e| format!(" ({e})"));
            parts.push(format!("end of turn → commit {ms} ms{by}"));
        }
        if let Some(ms) = st.asr_ms {
            parts.push(format!("ASR {ms} ms"));
        }
        if let Some(ms) = st.tools_list_ms {
            parts.push(format!("MCP listing {ms} ms"));
        }
        if let Some(ms) = st.first_token_ms {
            parts.push(format!("LLM first token {ms} ms"));
        }
        if let Some(ms) = st.reasoning_ms {
            parts.push(format!("of it reasoning {ms} ms"));
        }
        if let Some(ms) = st.first_clause_ms {
            parts.push(format!("first clause {ms} ms"));
        }
        if let Some(ms) = st.first_audio_ms {
            parts.push(format!("TTS first audio {ms} ms"));
        }
        if let Some(ms) = st.tools_ms {
            let n = st.tool_calls;
            let calls = if n == 1 { "call" } else { "calls" };
            parts.push(format!("MCP tools {ms} ms ({n} {calls})"));
        }
        let mut out = format!("{status:?}").to_lowercase();
        if !parts.is_empty() {
            let _ = write!(out, " — {}", parts.join(", "));
        }
        match st.first {
            Some((what, ms)) => {
                let _ = write!(out, "; first {what} at {ms} ms");
            }
            None if st.no_audio => out.push_str("; no audio reached the client"),
            None => {}
        }
        let _ = write!(out, "; total {} ms from {}", st.total_ms, st.from);
        match (st.held, st.transcript_wait_ms) {
            (true, Some(ms)) => {
                let _ = write!(out, "; input=audio held={ms} ms");
            }
            (true, None) => out.push_str("; input=audio, never released"),
            (false, _) => {}
        }
        out
    }
}

/// One response's stages ([`Timing::stages`]), each measured from the one
/// before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Stages {
    pub end_of_turn_ms: Option<u64>,
    pub asr_ms: Option<u64>,
    /// How long it waited for the session's MCP listings.
    pub tools_list_ms: Option<u64>,
    pub first_token_ms: Option<u64>,
    /// How long the chat model reasoned: part of `first_token_ms`.
    pub reasoning_ms: Option<u64>,
    pub first_clause_ms: Option<u64>,
    /// TTS: the first clause to its audio.
    pub first_audio_ms: Option<u64>,
    /// How long its server-side calls ran, first start to last end.
    pub tools_ms: Option<u64>,
    /// How many ran.
    pub tool_calls: u32,
    /// What reached the client first, and when, from [`Self::from`].
    pub first: Option<(&'static str, u64)>,
    /// A spoken response that produced tokens and no audio.
    pub no_audio: bool,
    pub total_ms: u64,
    /// Where the line counts from.
    pub from: &'static str,
    /// How long a held response's first output waited for the release (0
    /// when nothing waited); `None` for a response not held, or never
    /// released.
    pub transcript_wait_ms: Option<u64>,
    /// It heard the user's audio, and started held.
    pub held: bool,
}

impl Core {
    /// A response is launched, and renders every committed turn: the latest
    /// is its own now, even one committed after it was created (module doc).
    pub(super) fn timing_launched(&mut self) {
        let turn = self.last_turn.take();
        if let Some(active) = self.active.as_mut() {
            active.timing.launched = Some(Instant::now());
            if turn.is_some() {
                active.timing.turn = turn;
            }
        }
    }

    /// No response will answer the `owed` turns (noise, failed
    /// transcriptions): the latest committed turn, if one of them, is no
    /// later response's (module doc).
    pub(in crate::realtime) fn timing_unanswered(&mut self, owed: &[String]) {
        self.last_turn.take_if(|t| owed.contains(&t.item_id));
    }

    /// `item_id`'s transcript is in and has no words (noise, or a failed
    /// transcription). Owed an automatic response, its debt's decision drops
    /// it (`pending`); otherwise — `create_response` off, a client's commit
    /// — that decision never comes, and it is dropped here (A2 review 5).
    /// A response the client already asked for took its turn when it was
    /// created, and keeps it.
    pub(in crate::realtime) fn timing_no_words(&mut self, item_id: &str) {
        if !self.pending_owes(item_id) {
            self.timing_unanswered(&[item_id.to_string()]);
        }
    }

    /// The §11 timing line of `active`, which ended now with `status` — and,
    /// for a bound session, the same moments as its reply's timing and
    /// `lmgw.response.timing` (chat-voice design §8.7), measured to the same
    /// end.
    pub(super) fn log_timing(&mut self, active: &Active, status: ResponseStatus) {
        let end = Instant::now();
        tracing::info!(
            "realtime {}: response {} {}",
            self.id(),
            active.output.id,
            active.timing.line(status, end)
        );
        self.bound_ended(active, status, end);
    }

    /// An MCP listing is in flight: a response not launched yet waits for
    /// it (realtime-server-tools §1.2) — from now, unless it already did.
    pub(in crate::realtime) fn timing_tools_waiting(&mut self) {
        if let Some(t) = self
            .active
            .as_mut()
            .filter(|a| !a.phase.launched())
            .map(|a| &mut a.timing)
        {
            t.tools_wait.get_or_insert_with(Instant::now);
            t.tools_listed = None;
        }
    }

    /// The session's listings are all in: a response that waited for them
    /// stops waiting now.
    pub(in crate::realtime) fn timing_tools_listed(&mut self) {
        if let Some(t) = self
            .active
            .as_mut()
            .filter(|a| !a.phase.launched())
            .map(|a| &mut a.timing)
            .filter(|t| t.tools_wait.is_some())
        {
            t.tools_listed = Some(Instant::now());
        }
    }

    /// `item_id`'s transcript is in: its turn's moment, wherever its timing
    /// is kept — the next response's, or the waiting one's.
    pub(in crate::realtime) fn timing_transcribed(&mut self, item_id: &str) {
        let now = Instant::now();
        let turns = [
            self.last_turn.as_mut(),
            self.active.as_mut().and_then(|a| a.timing.turn.as_mut()),
        ];
        for t in turns.into_iter().flatten() {
            if t.item_id == item_id {
                t.transcribed = Some(now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn timing(turn: Option<TurnTiming>, speaks: bool, created: Instant) -> Timing {
        Timing {
            created,
            ..Timing::new(turn, speaks)
        }
    }

    #[test]
    fn a_spoken_turn_has_every_stage_and_a_typed_one_the_subset() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let turn = TurnTiming {
            item_id: "item_1".into(),
            speech_end: Some(at(0)),
            committed: at(510),
            transcribed: Some(at(570)),
            ended_by: None,
        };
        let spoken = Timing {
            launched: Some(at(572)),
            first_token: Some(at(600)),
            first_clause: Some(at(690)),
            first_audio: Some(at(760)),
            ..timing(Some(turn), true, at(571))
        };
        assert_eq!(
            spoken.line(ResponseStatus::Completed, at(4000)),
            "completed — end of turn → commit 510 ms, ASR 60 ms, LLM first token 28 ms, first \
             clause 90 ms, TTS first audio 70 ms; first audio at 760 ms; total 4000 ms from the \
             end of speech"
        );

        // A `semantic_vad` turn names the rule that ended it (§6.3).
        use super::super::super::turn::semantic::{EndRule, TurnEnd};
        let ended = TurnTiming {
            item_id: "item_2".into(),
            speech_end: Some(at(0)),
            committed: at(260),
            transcribed: None,
            ended_by: Some(TurnEnd {
                rule: EndRule::Threshold { p: 0.98 },
                held: false,
            }),
        };
        assert!(timing(Some(ended), true, at(261))
            .line(ResponseStatus::Completed, at(900))
            .starts_with(
                "completed — end of turn → commit 260 ms (semantic_vad threshold, p 0.98)"
            ));

        let typed = Timing {
            launched: Some(at(1)),
            first_token: Some(at(26)),
            first_text: Some(at(26)),
            ..timing(None, false, at(0))
        };
        assert_eq!(
            typed.line(ResponseStatus::Cancelled, at(300)),
            "cancelled — LLM first token 25 ms; first text at 26 ms; total 300 ms from \
             response.created"
        );
    }

    /// realtime-server-tools §1.2: the wait for MCP listings is a stage of
    /// its own, between the turn and the model call.
    #[test]
    fn a_wait_for_mcp_listings_is_timed() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let waited = Timing {
            tools_wait: Some(at(0)),
            tools_listed: Some(at(840)),
            launched: Some(at(841)),
            first_token: Some(at(900)),
            first_text: Some(at(900)),
            ..timing(None, false, at(0))
        };
        assert_eq!(waited.stages(at(1000)).tools_list_ms, Some(840));
        assert_eq!(
            waited.line(ResponseStatus::Completed, at(1000)),
            "completed — MCP listing 840 ms, LLM first token 59 ms; first text at 900 ms; total \
             1000 ms from response.created"
        );
        // Cancelled while it waited: up to its end.
        let cut = Timing {
            tools_wait: Some(at(10)),
            ..timing(None, false, at(0))
        };
        assert_eq!(cut.stages(at(300)).tools_list_ms, Some(290));
        // A response that never waited says nothing of it.
        assert_eq!(timing(None, false, at(0)).stages(at(9)).tools_list_ms, None);
    }

    /// realtime-server-tools §2.4: the server-side calls' run, first start
    /// to last end, and how many ran.
    #[test]
    fn server_side_calls_are_timed() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut ran = Timing {
            launched: Some(at(1)),
            first_token: Some(at(40)),
            first_call: Some((at(40), "MCP call")),
            ..timing(None, false, at(0))
        };
        ran.tool_running(at(60));
        ran.tool_running(at(61));
        ran.tool_done(at(300));
        ran.tool_done(at(180));
        let st = ran.stages(at(400));
        assert_eq!((st.tools_ms, st.tool_calls), (Some(240), 2));
        assert_eq!(
            ran.line(ResponseStatus::Completed, at(400)),
            "completed — LLM first token 39 ms, MCP tools 240 ms (2 calls); first MCP call at \
             40 ms; total 400 ms from response.created"
        );
        // Cut off running: up to the end.
        let mut cut = timing(None, false, at(0));
        cut.tool_running(at(10));
        assert_eq!(cut.stages(at(50)).tools_ms, Some(40));
        assert!(cut
            .line(ResponseStatus::Cancelled, at(50))
            .contains("MCP tools 40 ms (1 call)"));
        // None ran: nothing said.
        assert_eq!(timing(None, false, at(0)).stages(at(9)).tools_ms, None);
    }

    #[test]
    fn reasoning_is_timed_within_the_first_token() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let reasoned = Timing {
            launched: Some(at(1)),
            first_reasoning: Some(at(30)),
            first_token: Some(at(830)),
            first_text: Some(at(830)),
            ..timing(None, false, at(0))
        };
        let st = reasoned.stages(at(900));
        assert_eq!((st.first_token_ms, st.reasoning_ms), (Some(829), Some(800)));
        assert_eq!(
            reasoned.line(ResponseStatus::Completed, at(900)),
            "completed — LLM first token 829 ms, of it reasoning 800 ms; first text at 830 ms; \
             total 900 ms from response.created"
        );
        // Stopped while it reasoned: up to the end.
        let stopped = Timing {
            launched: Some(at(1)),
            first_reasoning: Some(at(30)),
            ..timing(None, true, at(0))
        };
        assert_eq!(stopped.stages(at(500)).reasoning_ms, Some(470));
        // A model that did not reason says nothing of it.
        assert_eq!(timing(None, true, at(0)).stages(at(9)).reasoning_ms, None);
    }

    #[test]
    fn first_names_what_reached_the_client() {
        // Package A review #7: a spoken response without audio named its
        // first token "first text", which the client never got.
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let called = Timing {
            launched: Some(at(1)),
            first_token: Some(at(40)),
            first_call: Some((at(40), "function call")),
            ..timing(None, true, at(0))
        };
        assert_eq!(
            called.line(ResponseStatus::Completed, at(90)),
            "completed — LLM first token 39 ms; first function call at 40 ms; total 90 ms from \
             response.created"
        );
        let cut = Timing {
            launched: Some(at(1)),
            first_token: Some(at(40)),
            ..timing(None, true, at(0))
        };
        assert_eq!(
            cut.line(ResponseStatus::Cancelled, at(60)),
            "cancelled — LLM first token 39 ms; no audio reached the client; total 60 ms from \
             response.created"
        );
        // A text response's call is not its "first text" either.
        let typed_call = Timing {
            launched: Some(at(1)),
            first_token: Some(at(30)),
            first_call: Some((at(30), "function call")),
            ..timing(None, false, at(0))
        };
        assert!(typed_call
            .line(ResponseStatus::Completed, at(50))
            .contains("; first function call at 30 ms"));
    }
}
