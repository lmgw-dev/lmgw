//! The input audio buffer's events, as the session core handles them
//! (realtime design §2.3, §4.2, §4.3, §5.2, §6).
//!
//! One voice turn under turn detection goes out as:
//! `speech_started` → `speech_stopped` → `committed` → user item `added`
//! (`input_audio`, `transcript: null`) → the ASR call → input transcription
//! `completed` (when the session asked for transcription events) → user item
//! `done` with the transcript → the automatic response, when
//! `create_response` is on and the transcript has words. `speech_started`
//! names the item the turn *will* become, and every later event names the
//! same one.
//!
//! **Every turn is announced and committed**, whenever it starts — or
//! deliberately not a turn at all: speech inside the client's playing
//! window must earn it through the barge-in gate, and a backchannel that
//! does not is dropped and logged; with `session.lmgw.half_duplex` input in
//! the window is not listened to (§6.4, `audio_in`, `turn::arbiter`). A turn
//! that starts while a response is in progress interrupts it (`judged`,
//! `lifecycle::interrupt`). Speech in the Pending phase (committed, no
//! `response.created` yet) defers the automatic response to the next commit,
//! which it then answers along with the turns before it (§4.3,
//! `lifecycle::pending`).
//!
//! Manual turns (`turn_detection: null`, §6.6): `input_audio_buffer.commit`
//! commits the buffer and answers no response by itself — `response.create`
//! does, and waits for the transcript (`lifecycle`).

use super::asr::{self, AsrResolution};
use super::audio::pcm::decode_pcm16;
use super::audio_in::{Appended, DetectorDown};
use super::expressive::{self, resolve_style, Asked};
use super::lifecycle::TurnTiming;
use super::policy;
use super::protocol::{
    ContentPart, ErrorObject, Item, ItemStatus, MessageItem, Role, ServerEvent, Session,
    ITEM_OBJECT,
};
use super::session::Core;
use super::turn::semantic::TurnEnd;
use super::voice::VoiceOutcome;
use super::warm::{Speaks, Warm, WarmMode};
use crate::telemetry::RequestClass;

mod check_alias;
mod detection;
mod judged;
mod semantic;
mod transcript;
mod word_check;
pub(in crate::realtime) use word_check::language as session_language;

pub(super) use check_alias::vet_client as vet_client_check_alias;
use detection::create_response;
pub(super) use detection::interrupt_response;
pub(crate) use detection::{
    barge_params, detector_params, log_turn_detection, semantic_rule, warn_unusable_semantic_rows,
    Barge,
};
pub(super) use transcript::{attempted, has_words, transcripts_failed};
pub(super) use word_check::{check_alias, own_check_alias};

/// The turn the detector has open.
pub(crate) struct OpenTurn {
    /// The item it becomes, named by `speech_started` already.
    pub item_id: String,
    /// The word check's alias, when the check that let this turn through
    /// heard words in it: the turn is transcribed once more with it, should
    /// its own transcript come back empty from another alias (§6.4, N3).
    pub heard_by: Option<String>,
}

impl OpenTurn {
    /// A turn no word check let through.
    pub(crate) fn new(item_id: String) -> Self {
        Self {
            item_id,
            heard_by: None,
        }
    }
}

/// The session's `audio.input.transcription.model` (§5.2).
fn transcription_model(s: &Session) -> Option<&str> {
    s.audio
        .as_ref()?
        .input
        .as_ref()?
        .transcription
        .as_ref()?
        .model
        .as_deref()
}

impl Core {
    /// `input_audio_buffer.append`, which arrived at `arrived`: the
    /// detector places its samples on the wall clock by that, to judge them
    /// against the playing window (§6.4). Audio that does not decode is an
    /// `error`, and the session goes on.
    pub(super) async fn input_append(
        &mut self,
        event_id: Option<&str>,
        audio: &str,
        arrived: tokio::time::Instant,
    ) {
        let pcm = match decode_pcm16(audio) {
            Ok(p) => p,
            Err(e) => {
                return self.error(
                    ErrorObject::invalid("invalid_value", e.to_string())
                        .with_param("audio")
                        .for_event(event_id),
                )
            }
        };
        self.note_margin();
        let listen = self.listen();
        let appended = self.input.append(&pcm, arrived, &listen).await;
        drop(pcm);
        self.on_appended(appended, None, None);
    }

    /// What the detector made of an append — or of a word check's verdict
    /// (`word_check`), whose note the turn's start line carries, and whose
    /// alias the turn keeps when it heard words (`heard_by`, N3): the
    /// deliberate non-turns logged, the turn events in order, a detector
    /// that went down, and the word checks it asked for.
    pub(super) fn on_appended(
        &mut self,
        appended: Appended,
        note: Option<&str>,
        heard_by: Option<&str>,
    ) {
        for d in appended.dropped {
            self.log_dropped(d);
        }
        for onset_ms in appended.promoted {
            tracing::info!(
                "realtime {}: speech from {onset_ms} ms of the input, which the barge-in gate \
                 passed during playback, went on past the answer's end before its words were \
                 known — the answer has played out, so it is a normal turn",
                self.id()
            );
        }
        for d in appended.events {
            self.on_judged_noted(d, note, heard_by);
        }
        let down = appended.down.is_some();
        if let Some(down) = appended.down {
            self.detector_down(down);
        }
        // A detector that went down took the turns being checked with it:
        // their checks would decide nothing (E5) — and the pauses with them.
        if !down {
            for c in appended.checks {
                self.word_check(c);
            }
            for s in appended.scores {
                self.scorer.score(s);
            }
        }
    }

    /// The detector went down, and the open turn's audio with it
    /// (`audio_in`'s module doc).
    ///
    /// A turn the client saw start never commits now: it is closed where the
    /// detector stopped with `speech_stopped` — the event that ends every
    /// announced turn — and nothing follows it. Left open, its item would
    /// stay the core's turn and an owed response it deferred would wait for
    /// a commit that never comes (package A review #5); that deferral ends
    /// here, as a clear ends it.
    pub(super) fn detector_down(&mut self, DetectorDown { why, at_ms }: DetectorDown) {
        tracing::warn!("realtime {}: turn detection stopped — {why}", self.id());
        if let Some(turn) = self.turn.take() {
            self.ob.send(ServerEvent::SpeechStopped {
                audio_end_ms: at_ms,
                item_id: turn.item_id,
            });
        }
        self.error(ErrorObject {
            kind: "server_error".into(),
            ..ErrorObject::invalid(
                "turn_detection_unavailable",
                format!(
                    "turn detection stopped: {why}. input_audio_buffer.clear starts it again, \
                     and so does switching turn_detection off and on; with turn_detection \
                     null, turns are committed by the client"
                ),
            )
        });
        self.pending_undefer();
    }

    /// `input_audio_buffer.commit` (§6.6): the uncommitted audio becomes a
    /// user turn. With turn detection on, that is the open turn, ended now.
    pub(super) fn input_commit(&mut self, event_id: Option<&str>) {
        // Before the buffer is taken: a client that fixes its
        // transcription model can commit the same audio again. A bound
        // session's turn reads the thread's ASR when its call starts, so a
        // chip fixed since the bind applies (WP11 binding review NIT 1).
        if self.asr.alias.is_none() && self.bound.is_none() {
            return self.asr_missing(event_id);
        }
        let Some(samples) = self.input.commit() else {
            return self.error(
                ErrorObject::invalid(
                    "input_audio_buffer_commit_empty",
                    if self.input.detecting() {
                        "there is no speech to commit: with turn detection on, the buffer holds \
                         the open turn only, and turns are committed when they end"
                    } else {
                        "the input audio buffer is empty"
                    },
                )
                .for_event(event_id),
            );
        };
        let turn = match self.turn.take() {
            Some(t) => t,
            None => OpenTurn::new(self.conversation.fresh_item_id(&self.ids)),
        };
        // The client decided the end: no detected end of turn to time.
        self.commit_turn(turn, samples, false, event_id, None, None);
    }

    /// `input_audio_buffer.clear`: the uncommitted audio and the open turn
    /// are gone, and the detector starts afresh.
    pub(super) fn input_clear(&mut self) {
        self.input.clear();
        self.turn = None;
        self.ob.send(ServerEvent::Cleared {});
        // The turn that deferred an owed response is gone with the audio.
        self.pending_undefer();
    }

    /// `asr_not_configured` (§5.2), with what resolution found.
    pub(super) fn asr_missing(&mut self, event_id: Option<&str>) {
        let why = self.asr.missing.as_deref().unwrap_or("no ASR alias");
        let e = ErrorObject::invalid(
            "asr_not_configured",
            format!("this turn cannot be transcribed: {why}"),
        )
        .with_param("session.audio.input.transcription.model")
        .for_event(event_id);
        self.error(e);
    }

    /// `committed`, the user item `added`, and its ASR call queued — with
    /// the word check's alias for a second call, when it heard words in the
    /// turn and is another alias than the turn's (§6.4, N3). `auto`: the
    /// turn's automatic response is pending (§4.3). `speech_end`: when the
    /// detector last heard its speech, and `ended_by` which part of
    /// `semantic_vad`'s rule ended it, for the timing line (§11).
    pub(super) fn commit_turn(
        &mut self,
        turn: OpenTurn,
        samples: Vec<i16>,
        auto: bool,
        event_id: Option<&str>,
        speech_end: Option<std::time::Instant>,
        ended_by: Option<TurnEnd>,
    ) {
        // A bound session with none at the bind: the call reads the
        // thread's as it is then (`Transcriber`), or fails the turn.
        let alias = match (self.asr.alias.clone(), self.bound.is_some()) {
            (Some(alias), _) => alias,
            (None, true) => String::new(),
            (None, false) => return self.asr_missing(event_id),
        };
        let OpenTurn {
            mut item_id,
            heard_by,
        } = turn;
        // The id was on the wire since `speech_started`; a client could have
        // claimed it for an item of its own since.
        if self.conversation.get(&item_id).is_some() {
            let fresh = self.conversation.fresh_item_id(&self.ids);
            tracing::warn!(
                "realtime {}: {item_id} was taken by a client item; this turn is {fresh}",
                self.id()
            );
            item_id = fresh;
        }
        let item = Item::Message(MessageItem {
            id: Some(item_id.clone()),
            object: Some(ITEM_OBJECT.into()),
            status: Some(ItemStatus::Completed),
            role: Role::User,
            content: vec![ContentPart::InputAudio {
                audio: None,
                transcript: None,
            }],
        });
        let previous = self.conversation.append(item.clone());
        self.ob.send(ServerEvent::Committed {
            previous_item_id: previous.clone(),
            item_id: item_id.clone(),
        });
        self.ob.send(ServerEvent::ItemAdded {
            previous_item_id: previous,
            item,
        });
        self.last_turn = Some(TurnTiming {
            item_id: item_id.clone(),
            speech_end,
            committed: std::time::Instant::now(),
            transcribed: None,
            ended_by,
        });
        let language = self.asr_language();
        let again = heard_by.filter(|check| *check != alias);
        // A turn the chat model hears: its WAV built once, for its ASR call
        // and the request (voice-audio-input design §3.1).
        let seconds = samples.len() as f64 / f64::from(super::audio::resample::INPUT_RATE);
        let (upload, heard) = self.commit_upload(&item_id, samples);
        self.transcriber
            .push_timed(item_id.clone(), alias, (upload, seconds), language, again);
        self.pending_commit(&item_id, auto, heard);
    }

    /// The ASR resolution for `next`: re-resolved, and the key re-checked
    /// for a new alias (§10.3), when the transcription model changed.
    pub(super) async fn update_asr(
        &mut self,
        next: &Session,
    ) -> Result<AsrResolution, ErrorObject> {
        let requested = transcription_model(next);
        if requested == transcription_model(&self.session) {
            return Ok(self.asr.clone());
        }
        // Fields, not `&self`, across the awaits: the core is `Send` but not
        // `Sync` (the resampler is not).
        let (state, ctx, current) = (&self.state, &self.ctx, &self.asr);
        const PARAM: &str = "session.audio.input.transcription.model";
        let gateway =
            |e: &crate::error::GatewayError| ErrorObject::from_gateway(e).with_param(PARAM);
        let asr = asr::resolve_asr(state, requested)
            .await
            .map_err(|e| gateway(&e))?;
        if let Some(alias) = asr.alias.as_deref().filter(|_| asr.alias != current.alias) {
            policy::check(state, ctx, alias, RequestClass::Audio)
                .await
                .map_err(|e| gateway(&e))?;
        }
        asr::log_resolution(self.session.id.as_deref().unwrap_or("?"), requested, &asr);
        Ok(asr)
    }

    /// Warm the session's models in the background (§9.1) — the TTS judged
    /// on the speech instructions the session's answers carry, and loaded
    /// with its voice (`warm`) — and the barge-in word check's model when it
    /// is one of its own (live run 3, D3): cold, its first check timed out
    /// while its container loaded, and the duration rule decided the first
    /// barge.
    ///
    /// **A bound session** warms the thread's models as they are now —
    /// re-read like each turn re-reads them (chat-voice §8.2, WP8 review
    /// m5) — and the word check's own alias when the client named one.
    pub(super) fn warm(&mut self) {
        let snap = self.state.snapshot();
        if let Some(b) = &self.bound {
            let (state, id, stages) = (self.state.clone(), b.thread_id, b.stages.clone());
            let check = barge_params(&self.session, &snap.settings.realtime)
                .words
                .then(|| own_check_alias(&self.session).map(str::to_string))
                .flatten();
            let sid = self.session.id.clone().unwrap_or_else(|| "?".into());
            let models = async move {
                let Some(thread) = crate::web::chat_voice::bound::thread(&state, id).await else {
                    return Vec::new();
                };
                let mut models = stages.now(&state, &thread).await;
                if let Some(alias) =
                    check.filter(|c| !models.iter().any(|m| m.stage() == "asr" && m.alias() == c))
                {
                    models.push(Warm::Model {
                        stage: "check",
                        alias,
                    });
                }
                models
            };
            return self
                .warmer
                .warm_later(&self.state, &sid, WarmMode::Background, models);
        }
        let voice = self.speech.tts.alias.clone().map(|alias| {
            let asked = Asked::of_session(&self.session, &snap.settings.realtime);
            Warm::Voice {
                alias,
                instructions: resolve_style(&self.speech.expressive, &asked).send,
                speaks: match &self.speech.voice {
                    VoiceOutcome::Resolved(v) => Some(Speaks {
                        voice: v.send.clone(),
                        language: session_language(&self.session)
                            .map(crate::audio::language::SpeechLanguage::Hint),
                        seed: Some(expressive::session_seed(&self.session, self.seed)),
                    }),
                    VoiceOutcome::Missing(_) | VoiceOutcome::NotFound(_) => None,
                },
            }
        });
        let asr = self.asr.alias.as_deref();
        let check = barge_params(&self.session, &snap.settings.realtime)
            .words
            .then(|| check_alias(&self.session, asr))
            .flatten()
            .filter(|c| Some(c.as_str()) != asr);
        let models = [
            ("chat", self.chat.alias.clone()),
            ("asr", self.asr.alias.clone()),
            ("check", check),
        ]
        .into_iter()
        .filter_map(|(stage, alias)| alias.map(|alias| Warm::Model { stage, alias }))
        .chain(voice)
        .collect();
        let sid = self.session.id.as_deref().unwrap_or("?");
        self.warmer
            .warm(&self.state, sid, WarmMode::Background, models);
    }
}
