//! What the page says about a voice model (chat-voice design §4.3): the
//! `state` frames of a press's warm, a chat turn and a read-aloud, turned
//! into the composer's status notes, and the hold and fallback lines a
//! thread's resolution gives before anything is recorded or spoken (§2.3) —
//! for the GPU hold and a benchmark run's lease alike ([`Block`]).
//! Pure, so it is tested natively.

use super::{StageResolved, VoiceResolved};

/// One `state` frame: `{stage, alias, state, ms, cause, answered_by,
/// reason, message}` — the shape clients share (`lmgw-api-types`).
pub(crate) use lmgw_api_types::chat_voice::ModelState;

/// How a note is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoteKind {
    /// Passing information, cleared by the next press or send.
    Info,
    /// Something under way ("loading …").
    Busy,
    /// Worth knowing: a fallback answered, the microphone is muted.
    Warn,
    /// The GPU hold (or a benchmark run) holds a voice model: amber, never
    /// an error (§4.3).
    Hold,
    Error,
}

impl NoteKind {
    pub(crate) fn class(self) -> &'static str {
        match self {
            NoteKind::Info => "info",
            NoteKind::Busy => "busy",
            NoteKind::Warn => "warn",
            NoteKind::Hold => "hold",
            NoteKind::Error => "err",
        }
    }
}

/// One line of the composer's voice status. `key` says what it is about
/// (`asr`, `tts`, `chat`, `dictation`, `read-aloud`); a newer note with the
/// same key replaces it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Note {
    pub key: String,
    pub kind: NoteKind,
    pub text: String,
    /// Where the note's fix is done, `(href, label)`: a link beside it
    /// ([`refusal_link`]). Only a link — nothing is done on the owner's
    /// behalf.
    pub link: Option<(&'static str, &'static str)>,
}

impl Note {
    pub(crate) fn new(key: &str, kind: NoteKind, text: impl Into<String>) -> Self {
        Self {
            key: key.to_string(),
            kind,
            text: text.into(),
            link: None,
        }
    }

    /// The note with `link` beside it.
    pub(crate) fn linked(mut self, link: Option<(&'static str, &'static str)>) -> Self {
        self.link = link;
        self
    }
}

/// The lead of a speech refusal that is fixed somewhere else in lmgw, by its
/// code — a turn's `error`, read-aloud's `speech_error`: the gateway's own
/// message follows it, naming the model (and the clip). `None` for any other
/// code.
pub(crate) fn speech_lead(code: &str) -> Option<&'static str> {
    match code {
        // `crate::audio::transcript` on the gateway: never another voice
        // instead, and nothing transcribed on the owner's behalf.
        "voice_needs_transcript" => Some("the voice clip needs a transcript first"),
        "audio_image_lacks_espeak" => Some("the audio.cpp image lacks eSpeak NG"),
        _ => None,
    }
}

/// Where the fix of a refusal or a voice problem is done, by its code: the
/// Audio lab's Transcribe for a clip without a transcript, the Backends
/// page's rebuild for an image without eSpeak NG.
pub(crate) fn refusal_link(code: &str) -> Option<(&'static str, &'static str)> {
    match code {
        "voice_needs_transcript" => Some(("/audio-lab", "Open the Audio lab")),
        "audio_image_lacks_espeak" => Some(("/backends", "Open Backends")),
        _ => None,
    }
}

/// [`refusal_link`] of the first problem of `stages` (§2.3 `problems`) —
/// the one [`blocker`] says.
pub(crate) fn problem_link(
    resolved: Option<&VoiceResolved>,
    stages: &[&str],
) -> Option<(&'static str, &'static str)> {
    resolved?
        .problems
        .iter()
        .find(|p| stages.contains(&p.stage.as_str()))
        .and_then(|p| refusal_link(&p.code))
}

/// A stage in the page's words.
pub(crate) fn stage_name(stage: &str) -> &'static str {
    match stage {
        "asr" => "speech-to-text",
        "tts" => "text-to-speech",
        "chat" => "chat model",
        _ => "voice model",
    }
}

/// The stages of `resolved` that run on the CPU, so the GPU hold does not
/// hold them (§4.3: the hold chip names them).
pub(crate) fn cpu_stages(resolved: Option<&VoiceResolved>) -> Vec<&'static str> {
    let Some(r) = resolved else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if r.asr.alias.is_some() && r.asr.cpu {
        out.push("speech-to-text");
    }
    if r.tts.alias.is_some() && r.tts.cpu {
        out.push("text-to-speech");
    }
    out
}

/// The hold chip's text for `alias`, naming what still serves on the CPU.
pub(crate) fn hold_text(alias: &str, cause: Option<&str>, cpu: &[&str]) -> String {
    let mut t = match cause {
        Some("benchmark") => format!("a benchmark run holds the GPU: {alias} waits until it ends"),
        _ => format!("GPU hold: local voice paused — {alias} waits for the GPU"),
    };
    if !cpu.is_empty() {
        t.push_str(&format!(
            "; {} still serve{} on the CPU",
            cpu.join(" and "),
            if cpu.len() == 1 { "s" } else { "" }
        ));
    }
    t
}

/// The note a `state` frame makes; `None` clears the stage's note (a model
/// that was up, with nothing to say).
pub(crate) fn note_of(s: &ModelState, cpu: &[&str]) -> Option<Note> {
    let key = s.stage.as_str();
    let what = stage_name(key);
    let alias = &s.alias;
    let (kind, text) = match s.state.as_str() {
        "loading" => (NoteKind::Busy, format!("loading {alias}…")),
        // No time: it was up, nothing to say.
        "ready" => (
            NoteKind::Info,
            format!("{alias} loaded in {}", super::spoken::ms_text(s.ms?)),
        ),
        "held" => (NoteKind::Hold, hold_text(alias, s.cause.as_deref(), cpu)),
        "fallback" => (
            NoteKind::Warn,
            match &s.answered_by {
                Some(b) => format!("{what}: {b} answers in place of {alias}"),
                None => format!("{what}: a fallback answers in place of {alias}"),
            },
        ),
        "skipped" => (
            NoteKind::Info,
            s.message.clone().unwrap_or_else(|| match s.reason.as_deref() {
                Some("does_not_fit") => {
                    format!("{alias} does not fit beside the other voice models; it loads when used")
                }
                Some("cannot_speak") => format!("{alias} cannot speak these replies"),
                _ => format!("{alias} was not loaded ahead: the GPU is full"),
            }),
        ),
        "failed" => (
            NoteKind::Error,
            format!(
                "{what} {alias} failed to load: {}",
                s.message.as_deref().unwrap_or("no reason given")
            ),
        ),
        _ => return None,
    };
    Some(Note::new(key, kind, text))
}

/// What blocks lmgw's own models now, from the titlebar's `vram` frame
/// (§2.3, WP7 review M2): the GPU hold blocks GPU rows; a benchmark run's
/// lease blocks every local row, a CPU row's included — both swap a request
/// to the row's fallback at resolve time (`Snapshot::resolve_for_request`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Block {
    pub hold: bool,
    pub benchmark: bool,
    /// No `vram` frame yet, or its stream ended: the hold is not known.
    pub unknown: bool,
}

impl Block {
    /// The words for what blocks `stage` now; `None` when nothing does.
    fn phrase(&self, stage: &StageResolved) -> Option<&'static str> {
        if stage.alias.is_none() || stage.local != Some(true) || !stage.managed {
            return None;
        }
        if self.hold && !stage.cpu {
            Some("under the GPU hold")
        } else if self.benchmark {
            Some("while a benchmark run holds the GPU")
        } else {
            None
        }
    }

    /// `stage` is swapped to its fallback (or refused) now: the control
    /// that sends to it turns amber.
    pub(crate) fn blocks(&self, stage: &StageResolved) -> bool {
        self.phrase(stage).is_some()
    }

    /// The words for what keeps a chat model lmgw runs from serving now —
    /// every chat row computes on the GPU, so the hold blocks it as a
    /// benchmark run's lease does — or `None` when nothing does (voice-audio-
    /// input design §2.2). Not known is not blocked.
    pub(crate) fn chat_phrase(&self) -> Option<&'static str> {
        if self.hold {
            Some("under the GPU hold")
        } else if self.benchmark {
            Some("while a benchmark run holds the GPU")
        } else {
            None
        }
    }
}

/// Where a stage's audio or text goes while something blocks it, said
/// before anything is recorded or spoken (§2.3): the fallback that answers
/// in its place (remote named), the refusal of one that cannot, or that
/// there is none. With the hold's state not known yet, what the hold would
/// do. `None` when nothing blocks it, and for an alias lmgw does not run.
pub(crate) fn under_block(stage: &StageResolved, b: Block) -> Option<String> {
    let which = match b.phrase(stage) {
        Some(p) => p,
        None if b.unknown && !stage.cpu && stage.local == Some(true) && stage.managed => {
            "if the GPU hold is on,"
        }
        None => return None,
    };
    if let Some(f) = &stage.fallback {
        return Some(format!(
            "{which} this goes to {}{}",
            f.alias,
            if f.local { "" } else { " (remote)" }
        ));
    }
    Some(match &stage.fallback_unusable {
        Some(u) => format!(
            "{which} this is refused: its fallback {} {}",
            u.alias, u.why
        ),
        None => format!("{which} this is refused: it has no fallback"),
    })
}

/// A refusal because lmgw's own models are blocked — `gpu_hold`, or
/// `gpu_benchmark` while a benchmark run holds the GPU — said as the amber
/// hold chip, never as an error (§4.3, WP7 review m1). `what` is the
/// feature ("dictation", "read-aloud"). `None` for any other code.
pub(crate) fn block_refusal(code: &str, what: &str, message: &str) -> Option<String> {
    match code {
        "gpu_hold" => Some(format!("GPU hold: {what} paused — {message}")),
        "gpu_benchmark" => Some(format!(
            "a benchmark run holds the GPU: {what} paused — {message}"
        )),
        _ => None,
    }
}

/// Where a turn's refusal is said: voice mode's panel, or the composer's
/// line under a text turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    Voice,
    Text,
}

/// The key a turn's refusal is said under. A `state` frame of the chat stage
/// (`held`, `failed`) has the same key, so the refusal that follows it
/// replaces it: one note per refusal, one amber chip for a hold (WP11 UI
/// review m6).
pub(crate) const TURN_KEY: &str = "chat";

/// A turn's refusal by its code (a chat turn's `error {code}`, a bound
/// response's `error`, a failed transcription): the note's kind and words,
/// what to do first and the gateway's own message after it. The hold and a
/// benchmark run are the amber chip ([`block_refusal`]); a GPU that was busy
/// is passing, so a warning. `None` for a code with nothing to add: the
/// message is said as it came.
pub(crate) fn turn_refusal(
    code: &str,
    surface: Surface,
    message: &str,
) -> Option<(NoteKind, String)> {
    let (what, again) = match surface {
        Surface::Voice => ("voice mode", "say it again"),
        Surface::Text => ("chat", "send it again"),
    };
    if let Some(t) = block_refusal(code, what, message) {
        return Some((NoteKind::Hold, t));
    }
    let (kind, lead) = match code {
        "context_length_exceeded" => (
            NoteKind::Error,
            "this conversation no longer fits the model's context: start a new conversation, or \
             delete old turns"
                .to_string(),
        ),
        "vram_queue_timeout" => (
            NoteKind::Warn,
            format!("the GPU was busy and the model did not get room in time: {again} in a moment"),
        ),
        "key_budget" => (
            NoteKind::Error,
            "the budget is spent: nothing more is answered until it is raised".to_string(),
        ),
        "unknown_alias" => (
            NoteKind::Error,
            "the conversation's model is not configured any more: pick another one for it"
                .to_string(),
        ),
        "upstream" => (
            NoteKind::Error,
            format!("the model answered with an error: {again}, or pick another model"),
        ),
        "asr_not_configured" => (
            NoteKind::Error,
            "no speech-to-text model for this conversation: pick one with the STT chip, or under \
             Settings → Chat → Voice"
                .to_string(),
        ),
        c @ ("voice_needs_transcript" | "audio_image_lacks_espeak") => (
            NoteKind::Error,
            speech_lead(c).unwrap_or_default().to_string(),
        ),
        _ => return None,
    };
    let message = message.trim();
    Some((
        kind,
        if message.is_empty() {
            lead
        } else {
            format!("{lead} — {message}")
        },
    ))
}

/// Where a stage goes when it is not on this machine at all.
pub(crate) fn remote_note(stage: &StageResolved) -> Option<String> {
    match (&stage.alias, stage.local) {
        (Some(a), Some(false)) => Some(format!("this goes to {a} (remote)")),
        _ => None,
    }
}

/// The blocker of a stage (§2.3 `problems`), as the gateway words it.
pub(crate) fn blocker(resolved: Option<&VoiceResolved>, stage: &str) -> Option<String> {
    resolved?
        .problems
        .iter()
        .find(|p| p.stage == stage)
        .map(|p| p.message.clone())
}

#[cfg(test)]
mod tests {
    use super::super::{FallbackResolved, FallbackUnusable, VoiceProblem};
    use super::*;
    use serde_json::json;

    fn frame(v: serde_json::Value) -> ModelState {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn loading_then_ready_with_its_time() {
        let n = note_of(
            &frame(
                json!({"stage": "asr", "alias": "nemotron-asr", "state": "loading", "ms": null}),
            ),
            &[],
        )
        .unwrap();
        assert_eq!(
            (n.kind, n.text.as_str()),
            (NoteKind::Busy, "loading nemotron-asr…")
        );
        let n = note_of(
            &frame(json!({"stage": "asr", "alias": "nemotron-asr", "state": "ready", "ms": 2140})),
            &[],
        )
        .unwrap();
        assert_eq!(n.text, "nemotron-asr loaded in 2.1 s");
        // Up already: nothing to say, the stage's note goes.
        assert!(note_of(
            &frame(json!({"stage": "asr", "alias": "a", "state": "ready", "ms": null})),
            &[]
        )
        .is_none());
    }

    #[test]
    fn a_hold_is_a_hold_naming_what_serves_on_the_cpu() {
        let n = note_of(
            &frame(
                json!({"stage": "tts", "alias": "qwen3-tts", "state": "held",
                          "cause": "gpu_hold", "ms": null}),
            ),
            &["speech-to-text"],
        )
        .unwrap();
        assert_eq!(n.kind, NoteKind::Hold);
        assert_eq!(
            n.text,
            "GPU hold: local voice paused — qwen3-tts waits for the GPU; speech-to-text still \
             serves on the CPU"
        );
        let n = note_of(
            &frame(json!({"stage": "asr", "alias": "a", "state": "held", "cause": "benchmark"})),
            &[],
        )
        .unwrap();
        assert!(n.text.starts_with("a benchmark run holds the GPU"));
    }

    #[test]
    fn a_fallback_and_a_failure_are_said() {
        let n = note_of(
            &frame(
                json!({"stage": "asr", "alias": "nemotron", "state": "fallback",
                          "answered_by": "openai/whisper-1"}),
            ),
            &[],
        )
        .unwrap();
        assert_eq!(
            (n.kind, n.text.as_str()),
            (
                NoteKind::Warn,
                "speech-to-text: openai/whisper-1 answers in place of nemotron"
            )
        );
        let n = note_of(
            &frame(json!({"stage": "chat", "alias": "g", "state": "failed", "message": "oom"})),
            &[],
        )
        .unwrap();
        assert_eq!(
            (n.kind, n.text.as_str()),
            (NoteKind::Error, "chat model g failed to load: oom")
        );
    }

    #[test]
    fn the_block_line_comes_before_recording() {
        let hold = Block {
            hold: true,
            ..Default::default()
        };
        let bench = Block {
            benchmark: true,
            ..Default::default()
        };
        let gpu = StageResolved {
            alias: Some("nemotron".into()),
            local: Some(true),
            managed: true,
            fallback: Some(FallbackResolved {
                alias: "openai/whisper-1".into(),
                local: false,
            }),
            ..Default::default()
        };
        assert_eq!(
            under_block(&gpu, hold).as_deref(),
            Some("under the GPU hold this goes to openai/whisper-1 (remote)")
        );
        assert!(hold.blocks(&gpu));
        assert_eq!(
            under_block(&gpu, Block::default()),
            None,
            "no block, no line"
        );
        assert!(!Block::default().blocks(&gpu));
        // A benchmark run's lease swaps it the same way.
        assert_eq!(
            under_block(&gpu, bench).as_deref(),
            Some("while a benchmark run holds the GPU this goes to openai/whisper-1 (remote)")
        );
        // A CPU row keeps serving under the hold, not under a benchmark run.
        let cpu = StageResolved {
            cpu: true,
            ..gpu.clone()
        };
        assert_eq!(under_block(&cpu, hold), None, "a CPU row keeps serving");
        assert!(!hold.blocks(&cpu));
        assert_eq!(
            under_block(&cpu, bench).as_deref(),
            Some("while a benchmark run holds the GPU this goes to openai/whisper-1 (remote)")
        );
        assert!(bench.blocks(&cpu));
        // Both on: the hold is named for a GPU row, the run for a CPU row.
        let both = Block {
            hold: true,
            benchmark: true,
            unknown: false,
        };
        assert!(under_block(&gpu, both)
            .unwrap()
            .starts_with("under the GPU hold"));
        assert!(under_block(&cpu, both)
            .unwrap()
            .starts_with("while a benchmark run"));
        let refused = StageResolved {
            fallback: None,
            fallback_unusable: Some(FallbackUnusable {
                alias: "x".into(),
                why: "is a GPU row too".into(),
            }),
            ..gpu.clone()
        };
        assert_eq!(
            under_block(&refused, hold).as_deref(),
            Some("under the GPU hold this is refused: its fallback x is a GPU row too")
        );
        // No fallback at all is said up front too (review m5).
        let none = StageResolved {
            fallback: None,
            ..gpu.clone()
        };
        assert_eq!(
            under_block(&none, hold).as_deref(),
            Some("under the GPU hold this is refused: it has no fallback")
        );
        // The hold not known yet: what it would do, for a GPU row.
        let unknown = Block {
            unknown: true,
            ..Default::default()
        };
        assert_eq!(
            under_block(&gpu, unknown).as_deref(),
            Some("if the GPU hold is on, this goes to openai/whisper-1 (remote)")
        );
        assert!(!unknown.blocks(&gpu), "not known is not amber");
        assert_eq!(under_block(&cpu, unknown), None);
        // A local server lmgw does not run, and an alias off this machine,
        // are never blocked.
        let unmanaged = StageResolved {
            managed: false,
            fallback: None,
            ..gpu
        };
        assert_eq!(under_block(&unmanaged, both), None);
        let remote = StageResolved {
            alias: Some("openai/whisper-1".into()),
            local: Some(false),
            ..Default::default()
        };
        assert_eq!(under_block(&remote, both), None);
        assert_eq!(
            remote_note(&remote).as_deref(),
            Some("this goes to openai/whisper-1 (remote)")
        );
    }

    #[test]
    fn a_hold_or_a_benchmark_refusal_is_the_hold_chip() {
        assert_eq!(
            block_refusal("gpu_hold", "dictation", "m").as_deref(),
            Some("GPU hold: dictation paused — m")
        );
        assert_eq!(
            block_refusal("gpu_benchmark", "read-aloud", "run 7").as_deref(),
            Some("a benchmark run holds the GPU: read-aloud paused — run 7")
        );
        assert_eq!(block_refusal("upstream_error", "dictation", "m"), None);
    }

    #[test]
    fn a_turns_refusal_is_worded_by_its_code() {
        let (k, t) = turn_refusal("gpu_hold", Surface::Voice, "'g' is a local model").unwrap();
        assert_eq!(
            (k, t.as_str()),
            (
                NoteKind::Hold,
                "GPU hold: voice mode paused — 'g' is a local model"
            )
        );
        let (k, t) = turn_refusal("gpu_benchmark", Surface::Text, "run 7").unwrap();
        assert_eq!(k, NoteKind::Hold);
        assert!(
            t.starts_with("a benchmark run holds the GPU: chat paused"),
            "{t}"
        );
        let (k, t) = turn_refusal(
            "context_length_exceeded",
            Surface::Voice,
            "'g': prompt 5000 tokens exceeds the per-request context of 4096 tokens",
        )
        .unwrap();
        assert_eq!(k, NoteKind::Error);
        assert!(t.starts_with("this conversation no longer fits"), "{t}");
        assert!(
            t.ends_with("context of 4096 tokens"),
            "the gateway's numbers stay: {t}"
        );
        // A busy GPU passes: a warning, and the surface's own "again".
        let (k, t) = turn_refusal("vram_queue_timeout", Surface::Voice, "").unwrap();
        assert_eq!(k, NoteKind::Warn);
        assert!(t.contains("say it again") && !t.contains(" — "), "{t}");
        let (_, t) = turn_refusal("vram_queue_timeout", Surface::Text, "m").unwrap();
        assert!(t.contains("send it again") && t.ends_with(" — m"), "{t}");
        for code in [
            "key_budget",
            "unknown_alias",
            "upstream",
            "asr_not_configured",
        ] {
            let (k, t) = turn_refusal(code, Surface::Voice, "why").unwrap();
            assert_eq!(k, NoteKind::Error, "{code}");
            assert!(t.ends_with(" — why"), "{code}: {t}");
        }
        // A refusal fixed elsewhere in lmgw: its lead, the gateway's words
        // (the model, the clip), and a link to where it is fixed.
        let msg = "the voice clip 'anna' has no transcript, and the text-to-speech model \
                   'audio/omni' cannot clone a clip without one — transcribe it in the Audio lab";
        let (k, t) = turn_refusal("voice_needs_transcript", Surface::Voice, msg).unwrap();
        assert_eq!(k, NoteKind::Error);
        assert!(
            t.starts_with("the voice clip needs a transcript first — "),
            "{t}"
        );
        assert!(
            t.contains("'anna'") && t.ends_with("in the Audio lab"),
            "{t}"
        );
        assert_eq!(
            refusal_link("voice_needs_transcript"),
            Some(("/audio-lab", "Open the Audio lab"))
        );
        let (k, t) = turn_refusal("audio_image_lacks_espeak", Surface::Text, "m").unwrap();
        assert_eq!(k, NoteKind::Error);
        assert_eq!(t, "the audio.cpp image lacks eSpeak NG — m");
        assert_eq!(
            refusal_link("audio_image_lacks_espeak").unwrap().0,
            "/backends"
        );
        assert_eq!(refusal_link("upstream"), None);
        // Anything else is said as it came.
        assert_eq!(
            turn_refusal("tts_not_configured", Surface::Voice, "m"),
            None
        );
        assert_eq!(turn_refusal("", Surface::Text, "m"), None);
    }

    #[test]
    fn the_blocker_is_the_gateways_message() {
        let r = VoiceResolved {
            problems: vec![VoiceProblem {
                stage: "asr".into(),
                code: "not_configured".into(),
                message: "no speech-to-text model is set — Settings → Chat → Voice".into(),
            }],
            ..Default::default()
        };
        assert!(blocker(Some(&r), "asr")
            .unwrap()
            .contains("Settings → Chat → Voice"));
        assert_eq!(blocker(Some(&r), "tts"), None);
        assert_eq!(cpu_stages(Some(&r)), Vec::<&str>::new());
        assert_eq!(problem_link(Some(&r), &["asr", "tts"]), None);
        // A clip without a transcript links to where it is transcribed.
        let r = VoiceResolved {
            problems: vec![VoiceProblem {
                stage: "tts".into(),
                code: "voice_needs_transcript".into(),
                message: "the voice clip 'anna' has no transcript".into(),
            }],
            ..Default::default()
        };
        assert_eq!(
            problem_link(Some(&r), &["tts"]),
            Some(("/audio-lab", "Open the Audio lab"))
        );
        assert_eq!(problem_link(Some(&r), &["asr"]), None);
    }
}
