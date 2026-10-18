//! The resolved voice as the API's types write it: `voice_resolved` of a
//! thread answer and of the settings answer (`lmgw_api_types::voice_resolved`).
//!
//! The logic tree (`VoiceConfig` and what it holds) stays crate-private and
//! is what the gateway computes with; the API's tree is what the document
//! describes and the dashboard reads. Each conversion destructures its
//! source without `..`: a field added to the logic tree does not compile
//! until it is on the wire (or deliberately left off it here), the way
//! `web::chat_wire` converts a thread. The test at the end writes both trees
//! and compares the JSON.

use lmgw_api_types::voice_resolved as api;

use super::super::audio_input::{AudioInput, Shown};
use super::{
    Fallback, FallbackUnusable, Problem, RealtimeAvailability, Source, Sourced, SpeechStyle, Stage,
    VoiceConfig, VoiceName,
};
use crate::web::chat_wire::{audio_input_of, input_path, turn_detection_of};

impl VoiceConfig {
    /// This resolution as the API writes it.
    pub(crate) fn wire(&self) -> api::VoiceResolved {
        let VoiceConfig {
            asr,
            tts,
            voice,
            speech_style,
            language,
            reply_language,
            language_notes,
            read_aloud,
            turn_detection,
            audio_input,
            seed,
            problems,
            realtime,
        } = self;
        api::VoiceResolved {
            asr: stage(asr),
            tts: stage(tts),
            voice: voice_name(voice),
            speech_style: style(speech_style),
            language: sourced(language, Clone::clone),
            reply_language: sourced(reply_language, Clone::clone),
            language_notes: language_notes.clone(),
            read_aloud: sourced(read_aloud, |on| *on),
            turn_detection: sourced(turn_detection, |t| turn_detection_of(*t)),
            audio_input: audio_input.as_ref().map(shown),
            seed: *seed,
            problems: problems.iter().map(problem).collect(),
            realtime: availability(realtime),
        }
    }
}

fn source(s: Source) -> api::VoiceSource {
    match s {
        Source::Thread => api::VoiceSource::Thread,
        Source::Profile => api::VoiceSource::Profile,
        Source::Chat => api::VoiceSource::Chat,
        Source::Realtime => api::VoiceSource::Realtime,
        Source::Row => api::VoiceSource::Row,
        Source::SpeechIn => api::VoiceSource::SpeechIn,
    }
}

fn sourced<T, U: Default>(s: &Sourced<T>, value: impl Fn(&T) -> U) -> api::Sourced<U> {
    let Sourced {
        value: held,
        source: from,
    } = s;
    api::Sourced {
        value: value(held),
        source: from.map(source),
    }
}

fn stage(s: &Stage) -> api::Stage {
    let Stage {
        alias,
        source: from,
        inherited,
        local,
        managed,
        cpu,
        fallback,
        fallback_unusable,
    } = s;
    api::Stage {
        alias: alias.clone(),
        source: from.map(source),
        inherited: inherited.clone(),
        local: *local,
        managed: *managed,
        cpu: *cpu,
        fallback: fallback.as_ref().map(|f| {
            let Fallback { alias, local } = f;
            api::Fallback {
                alias: alias.clone(),
                local: *local,
            }
        }),
        fallback_unusable: fallback_unusable.as_ref().map(|f| {
            let FallbackUnusable { alias, why } = f;
            api::FallbackUnusable {
                alias: alias.clone(),
                why: (*why).to_string(),
            }
        }),
    }
}

fn voice_name(v: &VoiceName) -> api::VoiceName {
    let VoiceName {
        name,
        source: from,
        inherits,
        note,
    } = v;
    api::VoiceName {
        name: name.clone(),
        source: from.map(source),
        inherits: inherits.clone(),
        note: note.clone(),
    }
}

fn style(s: &SpeechStyle) -> api::SpeechStyle {
    let SpeechStyle { text, source: from } = s;
    api::SpeechStyle {
        text: text.clone(),
        source: source(*from),
    }
}

fn problem(p: &Problem) -> api::VoiceProblem {
    let Problem {
        stage,
        code,
        message,
    } = p;
    api::VoiceProblem {
        stage: (*stage).to_string(),
        code: (*code).to_string(),
        message: message.clone(),
    }
}

fn availability(r: &RealtimeAvailability) -> api::RealtimeAvailability {
    let RealtimeAvailability {
        ok,
        code,
        reason,
        admin_tools,
    } = r;
    api::RealtimeAvailability {
        ok: *ok,
        code: code.map(str::to_string),
        reason: reason.clone(),
        admin_tools: *admin_tools,
    }
}

fn shown(s: &Shown) -> api::AudioInputShown {
    let Shown {
        value,
        source: from,
        verdict,
    } = s;
    api::AudioInputShown {
        value: audio_input_of(*value),
        source: source(*from),
        verdict: verdict_of(verdict),
    }
}

fn verdict_of(v: &AudioInput) -> api::AudioInputVerdict {
    // `via` and `lacks` are the bound session's logic, never on the wire.
    let AudioInput {
        path,
        model,
        why,
        lead,
        blocked,
        via: _,
        lacks: _,
    } = v;
    api::AudioInputVerdict {
        path: input_path(*path),
        model: model.clone(),
        why: why.clone(),
        lead: lead.map(str::to_string),
        blocked: blocked.as_deref().map(|b| Box::new(verdict_of(b))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AudioInputMode, InputPath, TurnDetection};
    use lmgw_api_types::chat_voice::LanguageNote;
    use serde_json::to_value;

    /// The two trees write the same JSON.
    fn same(cfg: &VoiceConfig) {
        let logic = to_value(cfg).unwrap();
        let wire = to_value(cfg.wire()).unwrap();
        assert_eq!(logic, wire);
        // ... and the API's reader takes back what the gateway wrote.
        let back: api::VoiceResolved = serde_json::from_value(logic).unwrap();
        assert_eq!(back, cfg.wire());
    }

    const SOURCES: [Source; 6] = [
        Source::Thread,
        Source::Profile,
        Source::Chat,
        Source::Realtime,
        Source::Row,
        Source::SpeechIn,
    ];

    fn blank() -> VoiceConfig {
        let none = || Stage {
            alias: None,
            source: None,
            inherited: None,
            local: None,
            managed: false,
            cpu: false,
            fallback: None,
            fallback_unusable: None,
        };
        VoiceConfig {
            asr: none(),
            tts: none(),
            voice: VoiceName {
                name: None,
                source: None,
                inherits: None,
                note: None,
            },
            speech_style: SpeechStyle {
                text: String::new(),
                source: Source::Chat,
            },
            language: Sourced {
                value: None,
                source: None,
            },
            reply_language: Sourced {
                value: None,
                source: None,
            },
            language_notes: Vec::new(),
            read_aloud: Sourced {
                value: false,
                source: Some(Source::Chat),
            },
            turn_detection: Sourced {
                value: TurnDetection::SemanticVad,
                source: Some(Source::Chat),
            },
            audio_input: None,
            seed: None,
            problems: Vec::new(),
            realtime: RealtimeAvailability {
                ok: true,
                code: None,
                reason: None,
                admin_tools: false,
            },
        }
    }

    fn verdict(path: InputPath, model: &str, why: Option<&str>) -> AudioInput {
        AudioInput {
            path,
            model: model.into(),
            why: why.map(str::to_string),
            lead: None,
            blocked: None,
            via: Vec::new(),
            lacks: false,
        }
    }

    /// Every `Option` that is `Some`, every list non-empty.
    fn full(from: Source) -> VoiceConfig {
        let stage = |tag: &str| Stage {
            alias: Some(format!("{tag}-alias")),
            source: Some(from),
            inherited: Some(format!("{tag}-inherited")),
            local: Some(true),
            managed: true,
            cpu: true,
            fallback: Some(Fallback {
                alias: format!("{tag}-fallback"),
                local: false,
            }),
            fallback_unusable: Some(FallbackUnusable {
                alias: format!("{tag}-unusable"),
                why: "does not resolve",
            }),
        };
        VoiceConfig {
            asr: stage("asr"),
            tts: stage("tts"),
            voice: VoiceName {
                name: Some("nova".into()),
                source: Some(from),
                inherits: Some("alloy".into()),
                note: Some("chosen for another model".into()),
            },
            speech_style: SpeechStyle {
                text: "calm".into(),
                source: from,
            },
            language: Sourced {
                value: Some("de".into()),
                source: Some(from),
            },
            reply_language: Sourced {
                value: Some("fr".into()),
                source: Some(from),
            },
            language_notes: vec![LanguageNote {
                stage: "tts".into(),
                alias: "tts-alias".into(),
                message: "takes no language".into(),
            }],
            read_aloud: Sourced {
                value: true,
                source: Some(from),
            },
            turn_detection: Sourced {
                value: TurnDetection::PushToTalk,
                source: Some(from),
            },
            audio_input: Some(Shown {
                value: AudioInputMode::On,
                source: from,
                verdict: AudioInput {
                    lead: Some("under the GPU hold"),
                    blocked: Some(Box::new(AudioInput {
                        lead: Some("while a benchmark run holds the GPU"),
                        ..verdict(InputPath::Transcript, "fb", Some("fb takes no audio"))
                    })),
                    via: vec!["m".into()],
                    lacks: true,
                    ..verdict(InputPath::Audio, "m", None)
                },
            }),
            seed: Some(u32::MAX),
            problems: vec![
                Problem {
                    stage: "asr",
                    code: "not_configured",
                    message: "no model".into(),
                },
                Problem {
                    stage: "tts",
                    code: "unresolved",
                    message: "does not resolve".into(),
                },
                Problem {
                    stage: "tts",
                    code: "voice_needs_transcript",
                    message: "needs a transcript".into(),
                },
            ],
            realtime: RealtimeAvailability {
                ok: false,
                code: Some("chat_thread_admin"),
                reason: Some("Voice mode is not available in Admin Chat".into()),
                admin_tools: true,
            },
        }
    }

    #[test]
    fn a_config_with_every_option_set_writes_the_same_json_from_each_source() {
        for from in SOURCES {
            same(&full(from));
        }
    }

    #[test]
    fn a_config_with_every_option_empty_writes_the_same_json() {
        let cfg = blank();
        same(&cfg);
        let v = to_value(cfg.wire()).unwrap();
        // The nulls the gateway sends are sent, not left out.
        for (path, key) in [
            ("asr", "alias"),
            ("asr", "fallback"),
            ("tts", "fallback_unusable"),
            ("voice", "note"),
            ("language", "source"),
            ("realtime", "code"),
        ] {
            assert!(
                v[path].get(key).is_some_and(|x| x.is_null()),
                "{path}.{key}"
            );
        }
        assert!(v["seed"].is_null() && v["audio_input"].is_null());
    }

    #[test]
    fn each_source_is_written_the_way_the_server_always_did() {
        for (from, name) in
            SOURCES
                .into_iter()
                .zip(["thread", "profile", "chat", "realtime", "row", "speech_in"])
        {
            assert_eq!(to_value(source(from)).unwrap(), name);
            assert_eq!(to_value(from).unwrap(), name);
        }
    }

    #[test]
    fn the_audio_input_verdict_has_lead_and_blocked_only_when_it_has_them() {
        let with = |lead: Option<&'static str>, blocked: Option<AudioInput>| {
            let mut cfg = blank();
            cfg.audio_input = Some(Shown {
                value: AudioInputMode::Off,
                source: Source::Thread,
                verdict: AudioInput {
                    lead,
                    blocked: blocked.map(Box::new),
                    ..verdict(InputPath::Transcript, "m", Some("audio input is off"))
                },
            });
            cfg
        };
        let forecast = verdict(InputPath::Audio, "fb", None);
        for (lead, blocked) in [
            (None, None),
            (Some("under the GPU hold"), None),
            (None, Some(forecast.clone())),
            (Some("under the GPU hold"), Some(forecast.clone())),
        ] {
            let cfg = with(lead, blocked.clone());
            same(&cfg);
            let a = &to_value(cfg.wire()).unwrap()["audio_input"];
            assert_eq!(a.get("lead").is_some(), lead.is_some());
            assert_eq!(a.get("blocked").is_some(), blocked.is_some());
            // `via` and `lacks` stay in the gateway.
            assert!(a.get("via").is_none() && a.get("lacks").is_none());
        }
    }

    #[test]
    fn a_forecast_inside_a_forecast_is_converted_all_the_way_down() {
        let mut cfg = blank();
        let inner = AudioInput {
            lead: Some("as a fallback"),
            ..verdict(InputPath::Transcript, "c", Some("c takes no audio"))
        };
        let middle = AudioInput {
            blocked: Some(Box::new(inner)),
            ..verdict(InputPath::Transcript, "b", Some("b takes no audio"))
        };
        cfg.audio_input = Some(Shown {
            value: AudioInputMode::On,
            source: Source::Chat,
            verdict: AudioInput {
                blocked: Some(Box::new(middle)),
                ..verdict(InputPath::Transcript, "a", Some("a takes no audio"))
            },
        });
        same(&cfg);
    }

    #[test]
    fn every_turn_detection_is_written_as_the_store_spells_it() {
        for t in [
            TurnDetection::SemanticVad,
            TurnDetection::ServerVad,
            TurnDetection::PushToTalk,
        ] {
            let mut cfg = blank();
            cfg.turn_detection.value = t;
            same(&cfg);
            assert_eq!(
                to_value(cfg.wire()).unwrap()["turn_detection"]["value"],
                t.as_str()
            );
        }
    }

    /// A resolution the gateway really computes (a thread of a default
    /// snapshot, a stage with no model) writes the same JSON too.
    #[test]
    fn a_resolved_default_thread_writes_the_same_json() {
        let snap = crate::config::Snapshot::default();
        let cfg = super::super::resolve(&snap, &crate::store::ChatThread::default());
        same(&cfg);
        assert_eq!(cfg.problems.len(), 2, "no model for either stage");
    }
}
