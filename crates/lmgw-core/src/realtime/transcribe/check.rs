//! The barge-in word check's ASR call (realtime design §6.4): the audio of a
//! turn the gate started but has not announced, transcribed at once.
//!
//! It is the turn call's own path (`super::call`): the key's per-call
//! policy check, the 16 kHz WAV, the gate, and one `request_logs` row of
//! class `audio`, labelled `realtime` — no hidden work. It is not queued
//! behind the committed turns' calls (one at a time, in commit order): the
//! answer keeps playing over the user until it is back.
//!
//! **Its timeout** (`barge_in_check_timeout_ms`) bounds the wait, not the
//! call: at the deadline the core is told, and the duration rule decides,
//! while the call runs on to its end and still writes its row. A call that
//! panics answers as a failure, so a check always reports back — an
//! unanswered one would keep its turn open. The session's end stops it,
//! like every ASR call of the session.

use std::time::Duration;

use tokio::time::Instant;

use super::super::audio::resample::INPUT_RATE;
use super::{caught, AsrMsg, Transcriber};
use crate::error::GatewayError;

/// A word check's answer, for the core.
pub(crate) struct CheckDone {
    /// The check it answers (`turn::arbiter::CheckRequest::id`).
    pub id: u64,
    /// The alias it transcribed with.
    pub alias: String,
    /// How long the checked audio was, in seconds.
    pub seconds: f64,
    /// How long the check took.
    pub took: Duration,
    pub heard: Result<String, CheckFailed>,
}

/// Why a check has no transcript.
pub(crate) enum CheckFailed {
    Call(GatewayError),
    /// Not back within this long.
    TimedOut(Duration),
}

impl Transcriber {
    /// Check `samples` (24 kHz) with `alias` for check `id`, at once;
    /// `timeout`: how long the core waits for it, `None` for no bound;
    /// `language`: the session's `audio.input.transcription.language`, when
    /// set (E4) — a bound session's thread language as it is now, as its
    /// turns use. `follows_asr`: `alias` is the session's ASR alias, not a
    /// check alias of its own — a bound session's check then transcribes
    /// with the thread's ASR alias as it is now, as its turns do (chat-voice
    /// §8.2; WP8 review NIT 9).
    pub fn check(
        &self,
        id: u64,
        alias: String,
        samples: Vec<i16>,
        timeout: Option<Duration>,
        (language, follows_asr): (Option<String>, bool),
    ) {
        let thread = self.thread;
        let (state, ctx, tx) = (self.state.clone(), self.ctx.clone(), self.tx.clone());
        let (stop, sid) = (self.signal.clone(), self.sid.clone());
        let seconds = samples.len() as f64 / f64::from(INPUT_RATE);
        tokio::spawn(async move {
            let started = Instant::now();
            let (mut alias, mut language) = (alias, language);
            if let Some(id) = thread {
                if let Some(now) = crate::web::chat_voice::bound::asr_now(&state, id).await {
                    language = now.language;
                    if let Some(a) = now.alias.filter(|_| follows_asr) {
                        alias = a;
                    }
                }
            }
            let what = format!("the barge-in word check {id}");
            let call = caught(
                &state,
                &ctx,
                &alias,
                samples.into(),
                &stop,
                (&sid, &what),
                language.as_deref(),
            );
            tokio::pin!(call);
            let done = |heard| {
                AsrMsg::Check(CheckDone {
                    id,
                    alias: alias.clone(),
                    seconds,
                    took: started.elapsed(),
                    heard,
                })
            };
            let call = async { call.await.map(|t| t.text) };
            tokio::pin!(call);
            let Some(timeout) = timeout else {
                let heard = call.await.map_err(CheckFailed::Call);
                let _ = tx.send(done(heard));
                return;
            };
            tokio::select! {
                heard = &mut call => {
                    let _ = tx.send(done(heard.map_err(CheckFailed::Call)));
                }
                () = tokio::time::sleep(timeout) => {
                    let _ = tx.send(done(Err(CheckFailed::TimedOut(timeout))));
                    // Run to its end: its row is written there.
                    let _ = call.await;
                }
            }
        });
    }
}
