//! The barge-in word check, in the session core (realtime design §6.4,
//! owner's decision 2026-10-01): each check the detector asks for goes at
//! once (`transcribe::check`) to the session's `barge_in_check_alias`, or
//! its ASR alias when that is empty ([`check_alias`], live run 2 W1), and
//! its answer decides whether the gate's turn is announced — and the answer
//! cut — or the answer plays on (`turn::arbiter::check`).
//!
//! - **Words** that are not all `realtime.backchannel_words`: cut, as
//!   without the check — the start line at INFO says what was heard.
//! - **Nothing, or only backchannel words, while the answer plays**: no
//!   `speech_started`; a DEBUG line per check. The detector keeps listening
//!   and checks again as the speech grows, and once more at its end.
//! - **Only backchannel words, but nothing plays any more** — no response,
//!   or one that would not be cut now (E1): "Ja" after "Soll ich das so
//!   machen?" is the user's reply. It is announced as a normal turn, and an
//!   INFO line says so.
//! - **No answer** — the call failed, or did not come back within
//!   `barge_in_check_timeout_ms` — or no ASR alias: the duration rule, so a
//!   cut. The start line says why.
//!
//! Words count only in the session's scripts (`barge_in_check_scripts`,
//! `turn::scripts`), and the check is told the session's
//! `audio.input.transcription.language` when it has one (E4), as the
//! turns' own calls are — its two-letter ISO 639-1 code, or nothing
//! ([`Core::asr_language`], fix package B6). A session whose language is written in a script
//! the list leaves out would hear no word during an answer: it says so
//! once, at WARN ([`Core::warn_check_scripts`]). A verdict whose turn is
//! gone — committed, cleared, promoted to a normal turn or discarded
//! since — decides nothing, and says so at DEBUG (E5).

use std::time::Duration;

use tokio::time::Instant;

use super::super::lifecycle::Interruption;
use super::super::protocol::Session;
use super::super::session::Core;
use super::super::transcribe::{CheckDone, CheckFailed};
use super::super::turn::arbiter::{CheckRequest, Verdict};
use super::super::turn::backchannel::{has_words_in, is_backchannel_in};
use super::super::turn::scripts::{covers, iso639_1, scripts_of};
use crate::config::RealtimeSettings;

/// The session's `barge_in_check_timeout_ms`; `None` for no bound.
fn timeout(s: &Session) -> Option<Duration> {
    let ms = s
        .lmgw
        .as_ref()
        .and_then(|l| l.barge_in_check_timeout_ms)
        .unwrap_or(0);
    (ms > 0).then(|| Duration::from_millis(u64::from(ms)))
}

/// The alias the word check transcribes with (module doc): the session's
/// `barge_in_check_alias`, else its ASR alias `asr`; `None` without
/// either — the duration rule decides then.
pub(in crate::realtime) fn check_alias(s: &Session, asr: Option<&str>) -> Option<String> {
    own_check_alias(s).or(asr).map(str::to_string)
}

/// The session's own `barge_in_check_alias`, when it names one.
pub(in crate::realtime) fn own_check_alias(s: &Session) -> Option<&str> {
    s.lmgw
        .as_ref()
        .and_then(|l| l.barge_in_check_alias.as_deref())
        .map(str::trim)
        .filter(|a| !a.is_empty())
}

/// The scripts whose words count: the session's, else the setting's.
fn check_scripts<'a>(s: &'a Session, settings: &'a RealtimeSettings) -> &'a [String] {
    s.lmgw
        .as_ref()
        .and_then(|l| l.barge_in_check_scripts.as_deref())
        .unwrap_or(&settings.barge_in_check_scripts)
}

/// The session's `audio.input.transcription.language`, when set — also the
/// language a spoken response is synthesized in (audio-class gap 7).
pub(in crate::realtime) fn language(s: &Session) -> Option<String> {
    s.audio
        .as_ref()?
        .input
        .as_ref()?
        .transcription
        .as_ref()?
        .language
        .clone()
        .filter(|l| !l.trim().is_empty())
}

impl Core {
    /// The language the session's ASR calls are told (fix package B6): the
    /// two-letter ISO 639-1 code its `audio.input.transcription.language`
    /// names ("de" for "de-DE"), or nothing — "german" or "deu" sent raw
    /// could make the model refuse every turn, and an unknown value is
    /// never the client's error. A value that names no code is noted at
    /// DEBUG.
    pub(super) fn asr_language(&self) -> Option<String> {
        let raw = language(&self.session)?;
        let code = iso639_1(&raw);
        if code.is_none() {
            tracing::debug!(
                "realtime {}: the transcription language '{raw}' names no two-letter ISO 639-1 \
                 code, so the ASR call is not told a language",
                self.id()
            );
        }
        code
    }

    /// Start the word check `c` asked for.
    pub(super) fn word_check(&mut self, c: CheckRequest) {
        let Some(alias) = check_alias(&self.session, self.asr.alias.as_deref()) else {
            // The check is only on with an alias; one dropped by a
            // session.update meanwhile leaves the duration rule.
            let appended = self.input.word_checked(c.id, Verdict::Cut);
            let note = "no word check: the session has no ASR alias (the duration rule decides)";
            return self.on_appended(appended, Some(note), None);
        };
        let follows_asr = own_check_alias(&self.session).is_none();
        self.transcriber.check(
            c.id,
            alias,
            c.samples,
            timeout(&self.session),
            (self.asr_language(), follows_asr),
        );
    }

    /// Whether anything the client hears would be cut by a turn starting
    /// now (E1).
    fn still_plays(&self) -> bool {
        matches!(self.interruption(Instant::now()), Interruption::Cuts { .. })
    }

    /// A word check's answer (module doc).
    pub(in crate::realtime) fn on_word_check(&mut self, done: CheckDone) {
        let took = format!(
            "{} ms for {:.2} s of speech",
            done.took.as_millis(),
            done.seconds
        );
        if !self.input.awaits_check(done.id) {
            let heard = match &done.heard {
                Ok(text) => format!("heard {text:?}"),
                Err(_) => "has no transcript".to_string(),
            };
            return tracing::debug!(
                "realtime {}: barge-in word check {} {heard} ({took}), but its turn is gone \
                 (committed, cleared, a normal turn or discarded since) — it decides nothing",
                self.id(),
                done.id
            );
        }
        // The alias that heard words, which a turn this verdict starts keeps
        // for a second transcription (N3, module doc).
        let mut heard_by = None;
        let (verdict, note) = match done.heard {
            Ok(text) => {
                let snap = self.state.snapshot();
                let words = &snap.settings.realtime.backchannel_words;
                let scripts = check_scripts(&self.session, &snap.settings.realtime);
                // What it heard, for the log: an empty transcript is no
                // backchannel word.
                let what = if has_words_in(&text, scripts) {
                    heard_by = Some(done.alias);
                    "backchannel words"
                } else {
                    "no words"
                };
                if !is_backchannel_in(&text, words, scripts) {
                    (Verdict::Cut, format!("words: {text:?} ({took})"))
                } else if self.still_plays() {
                    tracing::debug!(
                        "realtime {}: barge-in word check {} heard {text:?} ({took}) — {what}, \
                         the answer plays on",
                        self.id(),
                        done.id
                    );
                    (Verdict::Backchannel, String::new())
                } else {
                    tracing::info!(
                        "realtime {}: barge-in word check {} heard {text:?} ({took}) — {what}, \
                         but nothing plays any more: the user's reply, a normal turn",
                        self.id(),
                        done.id
                    );
                    (Verdict::Turn, String::new())
                }
            }
            Err(CheckFailed::TimedOut(t)) => (
                Verdict::Cut,
                format!(
                    "no word check: the ASR did not answer within {} ms \
                     (realtime.barge_in_check_timeout_ms); the duration rule decides",
                    t.as_millis()
                ),
            ),
            Err(CheckFailed::Call(e)) => (
                Verdict::Cut,
                format!("no word check: the ASR call failed ({e}); the duration rule decides"),
            ),
        };
        let appended = self.input.word_checked(done.id, verdict);
        let note = (!note.is_empty()).then_some(note);
        self.on_appended(appended, note.as_deref(), heard_by.as_deref());
    }

    /// Once per session (module doc): the transcription language is written
    /// in a script `barge_in_check_scripts` leaves out, so the word check
    /// hears none of the user's words during an answer.
    pub(in crate::realtime) fn warn_check_scripts(&mut self) {
        if self.scripts_warned {
            return;
        }
        let Some(lang) = language(&self.session) else {
            return;
        };
        let Some(needed) = scripts_of(&lang) else {
            return;
        };
        let snap = self.state.snapshot();
        let scripts = check_scripts(&self.session, &snap.settings.realtime);
        let missing: Vec<&str> = needed
            .iter()
            .copied()
            .filter(|s| !covers(scripts, s))
            .collect();
        if missing.is_empty() {
            return;
        }
        tracing::warn!(
            "realtime {}: the transcription language '{lang}' is written in {}, which \
             barge_in_check_scripts ({}) leaves out — the barge-in word check counts none of the \
             user's words during an answer, so no interruption cuts it; add {} to \
             realtime.barge_in_check_scripts or session.lmgw.barge_in_check_scripts, or empty \
             the list",
            self.id(),
            missing.join(" and "),
            scripts.join(", "),
            missing.join(", ")
        );
        self.scripts_warned = true;
    }
}
